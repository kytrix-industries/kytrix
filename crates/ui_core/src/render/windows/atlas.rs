//! 共享图集纹理（`SharedAtlas`）+ 每窗口上传器（`AtlasUploads`）
//! （格式与 UV 约定见 P02 §10）。
//!
//! 字形图集是 `R8_UNORM`（线性覆盖率），图片图集是 `B8G8R8A8_UNORM_SRGB`。两张纹理是**进程级
//! 共享资源**，由 `device::SharedGpu` 持有；每窗口只有自己的**上传路径**：
//!
//! - [`SharedAtlas`]：**texture array** + 当前页尺寸 + 层数 + 行 pitch + **CPU 影子页**（按层）。
//!   字形恒 1 层；图片的层数 = App 当前打开的图集页数（`ImageAtlasPager::page_count`，
//!   `render::AtlasArraySpec`），**随页数增长**（层数只增不减）；层号 = 图集页索引。
//!   懒分配：初始化只建 1×1×1 占位数组（SRV 始终指向有效资源，4 B 而不是 16 MiB），第一次
//!   整页上传或页档换代 / 层数增长时由 `SharedGpu::rebuild_atlas_pages` 重建。
//! - [`AtlasUploads`]（每窗口）：待上传层（每 kind 每层一份）+ 每 in-flight 槽的 staging。
//!   staging 必须是每窗口的：同一份共享纹理会被多个窗口的 GPU 线程并发上传，各窗口的 staging
//!   不能互相覆盖；每个槽的 staging 又只在本窗口该槽的 fence 完成前复用（与 `UploadArena`
//!   同一套 in-flight 语义）。同一帧的多层上传**共用一块 staging**、按各自偏移落位，录制时按
//!   层号写 `CopyTextureRegion` 的 subresource（D3D12 的数组子资源 = `mip + layer × mipLevels`，
//!   mip 0 时就是层号）。
//!
//! 并发模型：窗口的 GPU 线程在 `SharedGpu` 的图集**读锁**内录制并提交（页在录制期间不会变），
//! 页重建走**写锁**。写锁期间不可能出现新的提交（录制与提交都在读锁内）。C3 起页换代用**双代
//! 描述符组**：新 SRV 只写进没有在飞引用的那一代（判据 = 各窗口登记的代际弃用水位
//! `queue::StreamWatermark::generation_idle`），未空闲则本帧不换代（`Deferred`）—— 因此既不必
//! 等整条共享队列 idle（R08-6），也不会改写仍被在飞命令列表引用的描述符。
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
//! 16 MiB/层）。设备丢失后进程内恢复靠它把图集内容原样重传 —— 否则恢复出来的窗口只有占位纹理，
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
    paint::atlas::{AtlasKind, MAX_IMAGE_PAGES},
    render::{
        AtlasArraySpec, AtlasPageRequest,
        atlas::{DirtyRects, MAX_RESIDENT_STAGING_BYTES, RectChunk, texel_bytes},
        frame::MAX_FRAMES_IN_FLIGHT,
        merge_page_request,
        retire::GpuResourceHandle,
        windows::device::{SharedGpu, init_error_from},
    },
};

/// 初始化时的占位页（1×1×1）：让 SRV 在第一次上传之前也指向有效资源。
///
/// 占位数组只有 1 层（图片 4 B；旧实现是 `MAX_IMAGE_PAGES` 层 × 1×1 = 16 B，虽然也很小，
/// 但层数写死成常量正是「有图就占了 4 层」的根因）。
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

/// 一张共享图集页（texture array + 当前页尺寸 + 层数 + 行 pitch）。
///
/// 层数是**运行时值**：字形恒 1、图片 = App 当前打开的图集页数（`AtlasArraySpec`）。
struct SharedPage {
    resource: ID3D12Resource,
    page: u32,
    layers: u32,
    row_pitch: u32,
}

impl SharedPage {
    fn new(
        device: &ID3D12Device,
        kind: AtlasKind,
        spec: crate::render::AtlasArraySpec,
    ) -> Result<Self, EngineError> {
        let row_pitch = row_pitch_for(kind, spec.page);
        let resource = create_texture(device, spec.page, spec.layers, texture_format(kind))?;
        Ok(Self {
            resource,
            page: spec.page,
            layers: spec.layers,
            row_pitch,
        })
    }
}

/// 进程级共享的两张图集纹理（由 `SharedGpu` 的图集读写锁保护）。
pub(crate) struct SharedAtlas {
    glyph: SharedPage,
    image: SharedPage,
    /// 每张图集当前内容的 CPU 影子（`R08-4`：设备丢失后按它重传；见模块文档）。
    ///
    /// 影子按**层**保存（图片图集的层数随打开的页数增长；未上传过的层是 `None`）。放在
    /// `Mutex` 里而不是复用读写锁：影子只在**录制期**（已经持有图集读锁）被更新，两个窗口
    /// 并发上传同一层时写的是同一份内容，锁只保证 `Vec` 不被并发破坏。
    shadow: [Mutex<Option<ShadowAtlas>>; 2],
}

/// 一张图集纹理的 CPU 侧按层拷贝（每层紧密行，行宽 = `page × texel`）。
struct ShadowAtlas {
    page: u32,
    /// 每层是否已有内容（下标 = texture array 层号；字形只有下标 0）。
    layers: Vec<Option<Vec<u8>>>,
}

/// 设备恢复用的影子快照：页尺寸 + 每层整页字节（`None` = 该层从未上传过）。
pub(crate) struct ShadowAtlasSnapshot {
    page: u32,
    layers: Vec<Option<Vec<u8>>>,
}

impl ShadowAtlasSnapshot {
    /// 该图集的页边长（纹素）。
    pub(crate) fn page(&self) -> u32 {
        self.page
    }

    /// 每层的整页字节；下标 = texture array 层号（`None` = 该层从未上传过）。
    pub(crate) fn layers(&self) -> &[Option<Vec<u8>>] {
        &self.layers
    }
}

impl SharedAtlas {
    /// 建两张 1×1×1 占位 array；真实 array 在第一次整页上传 / 层数增长时由
    /// `SharedGpu::rebuild_atlas_pages` 创建。
    pub(crate) fn new(device: &ID3D12Device) -> Result<Self, EngineError> {
        let placeholder = crate::render::AtlasArraySpec {
            page: PLACEHOLDER_PAGE,
            layers: 1,
        };
        Ok(Self {
            glyph: SharedPage::new(device, AtlasKind::Glyph, placeholder)?,
            image: SharedPage::new(device, AtlasKind::Image, placeholder)?,
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

    /// 该图集 texture array 的当前层数（字形 1；图片 = App 当前打开的图集页数）。
    pub(crate) fn layers(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).layers
    }

    /// 该图集当前的 array 规格（页边长 + 层数）—— 上传器对齐 / 重建判据。
    pub(crate) fn spec(&self, kind: AtlasKind) -> crate::render::AtlasArraySpec {
        let page = self.page_ref(kind);
        crate::render::AtlasArraySpec {
            page: page.page,
            layers: page.layers,
        }
    }

    /// 仅测试：资源的 `DepthOrArraySize`（D3D12 对 `TEXTURE2D` 解释为数组长度）。
    ///
    /// 直接读 `ID3D12Resource::GetDesc()`，用来验证**生产重建入口**
    /// （`SharedGpu::rebuild_atlas_pages`）真的把资源建成了 N 层，而不是只看记账字段。
    #[cfg(test)]
    pub(crate) fn resource_array_size(&self, kind: AtlasKind) -> u16 {
        // SAFETY: 只读查询；资源由本结构持有。
        let desc = unsafe { self.page_ref(kind).resource.GetDesc() };
        desc.DepthOrArraySize
    }

    /// 该图集当前的 staging 行 pitch（字节，256 对齐）。
    pub(crate) fn row_pitch(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).row_pitch
    }

    /// 换 array：按新规格（页边长 + 层数）建 texture array 并替换本页；返回被换下来的旧纹理
    /// （调用方决定立即释放还是挂进延迟销毁表，R08-6 的慢路径）。
    ///
    /// 层数增长时旧层内容不会自动保留：调用方（App）在扩层帧对每个已打开页整页重传，
    /// 见 [`AtlasUploads::reconcile_pages`]。
    ///
    /// C3 起调用方（`SharedGpu::rebuild_atlas_pages`）把旧纹理按**当前描述符代**挂进
    /// `AtlasSwitch::retired_textures`，等该代重新启用（确认无在飞引用）时才释放 —— 旧纹理仍可能
    /// 被绑定旧代描述符的在飞命令列表采样。
    pub(crate) fn recreate(
        &mut self,
        device: &ID3D12Device,
        kind: AtlasKind,
        spec: crate::render::AtlasArraySpec,
    ) -> Result<ID3D12Resource, EngineError> {
        let next = SharedPage::new(device, kind, spec)?;
        let old = std::mem::replace(self.page_mut(kind), next);
        Ok(old.resource)
    }

    /// 把一次上传的字节写进 CPU 影子（R08-4）。
    ///
    /// `pixels` 是 `rects` 顺序**逐矩形紧密打包**的字节（与打包缓冲的布局同源）；页尺寸或层数
    /// 变化说明 App 换代 / 扩层，影子按新规格整页重建（整页上传是换代之后的唯一合法载荷，见
    /// [`AtlasUploads::reconcile_pages`]）。
    fn update_shadow(
        &self,
        kind: AtlasKind,
        layer: u16,
        page: u32,
        pixels: &[u8],
        rects: &DirtyRects,
    ) {
        let texel = texel_bytes(kind) as usize;
        let row_bytes = page as usize * texel;
        if row_bytes == 0 || pixels.len() != rects.byte_count(page, texel_bytes(kind)) {
            return;
        }
        let layers = self.layers(kind);
        let layer_index = usize::from(layer);
        let mut shadow = shadow_lock(&self.shadow[kind.index() as usize]);
        if shadow
            .as_ref()
            .is_none_or(|shadow| shadow.page != page || shadow.layers.len() != layers as usize)
        {
            *shadow = Some(ShadowAtlas {
                page,
                layers: (0..layers).map(|_| None).collect(),
            });
        }
        let Some(shadow) = shadow.as_mut() else {
            return;
        };
        let Some(layer_pixels) = shadow.layers.get_mut(layer_index) else {
            return;
        };
        if layer_pixels
            .as_ref()
            .is_none_or(|pixels| pixels.len() != row_bytes * page as usize)
        {
            *layer_pixels = Some(vec![0; row_bytes * page as usize]);
        }
        let Some(shadow_pixels) = layer_pixels.as_mut() else {
            return;
        };
        for chunk in rects.chunks(page, texel_bytes(kind)) {
            let width_bytes = chunk.width as usize * texel;
            for row in 0..chunk.height as usize {
                let source = chunk.offset + row * width_bytes;
                let destination = (chunk.y as usize + row) * row_bytes + chunk.x as usize * texel;
                if source + width_bytes <= pixels.len()
                    && destination + width_bytes <= shadow_pixels.len()
                {
                    shadow_pixels[destination..destination + width_bytes]
                        .copy_from_slice(&pixels[source..source + width_bytes]);
                }
            }
        }
    }

    /// 设备丢失恢复用：两张图集当前页 + 每层内容的快照（没有影子页的图集是 `None`）。
    pub(crate) fn shadow_snapshot(&self) -> [Option<ShadowAtlasSnapshot>; 2] {
        std::array::from_fn(|index| {
            shadow_lock(&self.shadow[index])
                .as_ref()
                .map(|shadow| ShadowAtlasSnapshot {
                    page: shadow.page,
                    layers: shadow.layers.clone(),
                })
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

fn shadow_lock(shadow: &Mutex<Option<ShadowAtlas>>) -> MutexGuard<'_, Option<ShadowAtlas>> {
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
    /// 该矩形来自哪一个待上传层（`AtlasStaging::layout` 的下标）。同一帧的多层共用一块
    /// staging，每层写入各自偏移，不能共用 offset 0（否则 GPU 只读到最后一层）。
    pending: usize,
    chunk: RectChunk,
    /// 目标 texture array 层号（`CopyTextureRegion` 的 subresource index，mip 0）。
    layer: u16,
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
    ///
    /// 同一 kind 的多层（图片图集一帧内最多 4 层）聚合进**同一次** staging 布局：各层
    /// 矩形按层号顺序排列、偏移连续，避免同一块 staging 上多层共用 offset 0（V-64 同类问题）。
    fn layout(&self, pendings: &[PendingUpload]) -> (Vec<StagingRect>, u32) {
        let texel = texel_bytes(self.kind);
        let mut layout = Vec::new();
        let mut end = 0u32;
        for (pending_index, pending) in pendings.iter().enumerate() {
            for chunk in pending.rects.chunks(pending.spec.page, texel) {
                debug_assert!(
                    chunk.x + chunk.width <= self.page && chunk.y + chunk.height <= self.page,
                    "atlas AOI chunk {chunk:?} is outside the {}-texel page",
                    self.page
                );
                let row_pitch = align_up(chunk.width * texel, PITCH_ALIGNMENT);
                let offset = align_up(end, PLACED_OFFSET_ALIGNMENT);
                end = offset + row_pitch * chunk.height;
                layout.push(StagingRect {
                    pending: pending_index,
                    chunk,
                    layer: pending.layer,
                    row_pitch,
                    offset,
                });
            }
        }
        (layout, end)
    }

    /// 在本帧命令列表里录制「staging 写入 + 逐矩形 `CopyTextureRegion`」。
    ///
    /// AOI（R06-3）：每个脏矩形单独一次拷贝，`PlacedFootprint.Width/Height` 与 `destination`
    /// 的 `(DstX, DstY)` 都取矩形自己的 `(x, y, w, h)` —— 页宽只出现在页缓冲的行 pitch 里，
    /// staging 的行 pitch 是**矩形宽**（单字形不再上传整页宽的行）。`destination` 的
    /// subresource index = 层号（D-9-c：多页图集 = texture array）。
    fn upload(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
        pendings: &[PendingUpload],
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        let (layout, total_bytes) = self.layout(pendings);
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
            self.upload_rect(list, resource, staging, &pendings[rect.pending], *rect);
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
                // D-9-c：目标 subresource = 层号（MipLevels = 1 ⇒ mip0 的数组子资源就是 layer）。
                SubresourceIndex: u32::from(rect.layer),
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

/// 待上传的一层内容（打包 `pixels` + 层号 + 页尺寸 + 脏矩形）。
struct PendingUpload {
    /// texture array 层号（字形恒 0；图片 = 图集页索引）。
    layer: u16,
    /// 入队时 App 要求的 array 规格（页边长 + 层数）。
    ///
    /// 层数 = 入队那一帧打开的图集页数：它可能**大于**共享 array 的当前层数（新页刚打开），
    /// 这时必须先把共享 array 扩层再录制（见 [`AtlasUploads::reconcile_pages`]）。
    spec: AtlasArraySpec,
    /// 入队时**共享图集**的页尺寸。
    ///
    /// 用来区分「新页」（App 换代后要重建共享纹理，`spec.page != queued_page`）与「旧页」
    /// （入队后别的窗口已经换代，`spec.page == queued_page` 且与当前共享页不同：上传作废）。
    queued_page: u32,
    pixels: Vec<u8>,
    rects: DirtyRects,
}

impl PendingUpload {
    /// 是否覆盖整页（首次上传 / 页换代 / 扩层之后的唯一合法载荷，见
    /// [`AtlasUploads::reconcile_pages`]）。
    fn covers_full_page(&self) -> bool {
        self.rects.covers_page(self.spec.page)
    }
}

/// 每窗口的图集上传路径：待上传层（每 kind 每层一份）+ 每 in-flight 槽 staging。
pub(crate) struct AtlasUploads {
    /// `[kind][layer]`：字形只有 `[0]`，图片 `0..MAX_IMAGE_PAGES`（页数上限；数组当前层数
    /// 更小，入队断言用 `spec.layers`）。
    pending: [[Option<PendingUpload>; MAX_IMAGE_PAGES as usize]; 2],
    staging: [Option<AtlasStaging>; 2],
}

impl AtlasUploads {
    pub(crate) fn new() -> Self {
        Self {
            pending: std::array::from_fn(|_| std::array::from_fn(|_| None)),
            staging: [None, None],
        }
    }

    /// 该图集是否有**任意层**的上传已入队、还没被命令列表消费（离屏目标的空闲判据）。
    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        self.pending[kind.index() as usize]
            .iter()
            .any(Option::is_some)
    }

    /// 测试：某图集的 staging（还没上传过时为 `None`）。
    #[cfg(test)]
    fn staging(&self, kind: AtlasKind) -> Option<&AtlasStaging> {
        self.staging[kind.index() as usize].as_ref()
    }

    /// 测试：某图集**已入队未录制**的层号（升序；诊断「扩层帧是否重传了每个已打开层」）。
    #[cfg(test)]
    pub(crate) fn pending_layers(&self, kind: AtlasKind) -> Vec<u16> {
        self.pending[kind.index() as usize]
            .iter()
            .enumerate()
            .filter_map(|(layer, slot)| {
                slot.as_ref()
                    .map(|_| u16::try_from(layer).expect("atlas layer index fits u16"))
            })
            .collect()
    }

    /// 入队一层的脏矩形（`pixels` 是 `rects` 顺序**逐矩形紧密打包**的字节，R06-3）。
    ///
    /// `spec` 是 App 要求的 array 规格（页边长 + 层数）：层号硬断言按**运行时层数**校验
    /// （图片 reopened 页数可以小于上限），不是 `MAX_IMAGE_PAGES` 常量。
    /// `pixels` 按**所有权**移动进待上传槽，避免槽 → 纹理之间再复制一次。长度必须等于
    /// [`DirtyRects::byte_count`]（release 也硬断言：越界读会读穿堆）。
    pub(crate) fn queue_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        spec: AtlasArraySpec,
        queued_page: u32,
        pixels: Vec<u8>,
        rects: DirtyRects,
    ) {
        assert!(
            u32::from(layer) < spec.layers,
            "atlas layer {layer} out of range for {kind:?}"
        );
        let page = spec.page;
        let expected = rects.byte_count(page, texel_bytes(kind));
        assert_eq!(
            pixels.len(),
            expected,
            "atlas payload size mismatch for {kind:?} layer {layer}: expected {expected} bytes for the dirty rects of a {page}² page, got {}",
            pixels.len()
        );
        let slot = &mut self.pending[kind.index() as usize][usize::from(layer)];
        assert!(
            slot.is_none(),
            "atlas payload slot must be consumed before the next upload (kind {kind:?}, layer {layer})"
        );
        *slot = Some(PendingUpload {
            layer,
            spec,
            queued_page,
            pixels,
            rects,
        });
    }

    /// 帧前对齐**共享图集 array**：丢弃已经不可能再被上传的过期增量，返回必须重建的 array。
    ///
    /// 共享 array 的规格（页边长 + 层数）由 App 的图集分配器决定，是进程级的：别的窗口一旦
    /// 换代 / 扩层，本窗口排队中的旧上传就不再成立。
    ///
    /// 两类重建：
    /// - **页换代**：整页 + 入队时共享页还不是它（`queued_page != spec.page`）⇒ App 换代；
    /// - **层数增长**：`spec.layers > current.layers` ⇒ App 开了新图集页。扩层帧 App 对**每个**
    ///   已打开页都整页重传，因此待上传的每一层都必须是整页；不是整页的（别的窗口换代后留下
    ///   的过期增量）直接丢弃（它会被整页内容覆盖）。
    ///
    /// 页已对齐且层数足够的上传保留到录制。层数只增不减：请求取待上传规格与当前层数的较大值，
    /// 同一 kind 只留一条请求。过期的「旧页」上传（入队时共享页就是它，之后被别的窗口换代）
    /// 无论整页还是增量都直接丢弃 —— 重建回旧页会让所有窗口的 tile 对尺寸错误的纹理采样。
    ///
    /// 调用方在 `SharedGpu` 的图集读锁内调用；返回非空时先释放读锁，
    /// `SharedGpu::rebuild_atlas_pages` 在写锁内落地新 array，然后重试。
    pub(crate) fn reconcile_pages(
        &mut self,
        current: [AtlasArraySpec; 2],
    ) -> Result<Vec<AtlasPageRequest>, EngineError> {
        let mut rebuild: Vec<AtlasPageRequest> = Vec::with_capacity(2);
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let current = current[index];
            // fail-closed（RBT-01 不变量 2）：**扩层请求必须每一层都带整页上传**。扩层会重建整个
            // 数组（旧层内容全部作废），只有「所有已打开层的整页内容都在同一次重传里入队」才
            // 允许扩张；否则这一帧直接拒绝（release 也生效，不是 debug_assert）——重建后让
            // 半成品层进 draw 比拒绝一帧糟得多。
            let pending_spec = self.pending[index]
                .iter()
                .flatten()
                .map(|pending| pending.spec)
                .fold(None, |acc: Option<AtlasArraySpec>, spec| {
                    Some(match acc {
                        Some(acc) => AtlasArraySpec {
                            page: spec.page,
                            layers: acc.layers.max(spec.layers),
                        },
                        None => spec,
                    })
                });
            if let Some(pending_spec) = pending_spec
                && pending_spec.layers > current.layers
            {
                for layer in 0..pending_spec.layers {
                    let full = self.pending[index]
                        .get(layer as usize)
                        .and_then(Option::as_ref)
                        .is_some_and(|pending| {
                            pending.spec.page == pending_spec.page && pending.covers_full_page()
                        });
                    if !full {
                        return Err(EngineError::AtlasGrowthIncomplete {
                            kind,
                            layer: u16::try_from(layer).unwrap_or(u16::MAX),
                            layers: pending_spec.layers,
                        });
                    }
                }
            }
            for layer in 0..self.pending[index].len() {
                let Some(pending) = &self.pending[index][layer] else {
                    continue;
                };
                let spec = pending.spec;
                if spec.page == current.page {
                    // 页已对齐：层数够 ⇒ 保留到录制；层数不够 ⇒ 扩层（层数只增不减）。
                    if spec.layers <= current.layers {
                        continue;
                    }
                    if !pending.covers_full_page() {
                        self.pending[index][layer] = None;
                        continue;
                    }
                    merge_page_request(&mut rebuild, kind, spec, current);
                    continue;
                }
                // 整页 + 「入队时共享页还不是它」= App 换代的**新页**：重建共享 array。
                // 其余都是别的窗口已经换代之后留下的过期上传：丢弃即可。
                if pending.covers_full_page() && pending.queued_page != spec.page {
                    merge_page_request(&mut rebuild, kind, spec, current);
                } else {
                    self.pending[index][layer] = None;
                }
            }
        }
        Ok(rebuild)
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
    /// 必须在 `reconcile_pages` 之后、录制之前调用：此时每个待上传层都等于共享页（层数已足够，
    /// 扩层已落地）。被换下来的旧 staging 句柄推进 `retired`（调用方按本帧序号 retire，R08-2）。
    pub(crate) fn sync_staging(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let Some(page) = self.pending[index]
                .iter()
                .flatten()
                .map(|pending| pending.spec.page)
                .next()
            else {
                continue;
            };
            debug_assert!(
                self.pending[index]
                    .iter()
                    .flatten()
                    .all(|pending| pending.spec.page == page),
                "all layers of one atlas must share the same page extent"
            );
            if self.staging[index]
                .as_ref()
                .is_some_and(|staging| staging.page == page)
            {
                continue;
            }
            let staging = AtlasStaging::new(shared, kind, page, atlas.row_pitch(kind))?;
            if let Some(old) = self.staging[index].replace(staging) {
                old.retire(retired);
            }
            debug_assert_eq!(atlas.page(kind), page, "图集页与待上传页必须已经对齐");
        }
        Ok(())
    }

    /// 在本帧命令列表里录制全部待上传层（每层 = staging 写入 + 逐区间 `CopyTextureRegion`），
    /// 同时把字节落进 CPU 影子页（R08-4 的恢复来源）。
    ///
    /// 同一 kind 的多层在一次调用里**聚合**：共用一块 staging、各层矩形各有偏移，逐矩形把
    /// `CopyTextureRegion` 的 subresource index 写成层号（多页 texture array）。
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
            let pendings: Vec<PendingUpload> = self.pending[index]
                .iter_mut()
                .filter_map(Option::take)
                .collect();
            if pendings.is_empty() {
                continue;
            }
            let page = atlas.page(kind);
            // 防御：页在录制期间被改（理论上被图集读锁排除）。丢掉而不是画错内容。
            if pendings.iter().any(|pending| pending.spec.page != page) {
                continue;
            }
            debug_assert!(
                pendings
                    .iter()
                    .all(|pending| u32::from(pending.layer) < atlas.layers(kind)),
                "pending layers must be inside the shared texture array"
            );
            // 影子页与 GPU 上传用同一份字节：设备丢失后按它重传整页（R08-4）。
            for pending in &pendings {
                atlas.update_shadow(
                    kind,
                    pending.layer,
                    pending.spec.page,
                    &pending.pixels,
                    &pending.rects,
                );
            }
            let Some(staging) = self.staging[index].as_mut() else {
                return Err(EngineError::AppInit {
                    detail: smol_str::format_smolstr!(
                        "atlas staging missing for {kind:?} page {page}"
                    ),
                });
            };
            debug_assert_eq!(staging.page, page);
            staging.upload(shared, atlas, list, slot, &pendings, &mut retired)?;
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

/// 把一块 UPLOAD staging 的整页内容拷进图集纹理的指定层（设备恢复用，R08-4）。
///
/// 调用方负责前后各一次状态过渡（`PIXEL_SHADER_RESOURCE → COPY_DEST → PIXEL_SHADER_RESOURCE`），
/// 并保证 `staging.pixels.len() == row_pitch × page`。
pub(crate) fn copy_page(
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    staging: &ID3D12Resource,
    kind: AtlasKind,
    page: u32,
    layer: u16,
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
            // texture array 的 mip0 子资源下标 = 层号（MipLevels = 1）。
            SubresourceIndex: u32::from(layer),
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

/// 建一张 `page × page`、`layers` 层（texture array）的图集纹理（占位页与真实页共用）。
fn create_texture(
    device: &ID3D12Device,
    page: u32,
    layers: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<ID3D12Resource, EngineError> {
    debug_assert!(layers > 0, "texture array must have at least one layer");
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(page),
        Height: page,
        // 图集是 texture array：层数是运行时值（字形 1、图片 = 当前打开的图集页数）。
        DepthOrArraySize: u16::try_from(layers).expect("atlas texture array layer count fits u16"),
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
            AtlasArraySpec, AtlasPageRequest,
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

    /// 图集 array 规格（测试页 + 层数）。
    fn spec(page: u32, layers: u32) -> AtlasArraySpec {
        AtlasArraySpec { page, layers }
    }

    /// 只含一个 kind 的重建请求（断言用）。
    fn request(kind: AtlasKind, page: u32, layers: u32) -> AtlasPageRequest {
        AtlasPageRequest {
            kind,
            spec: spec(page, layers),
        }
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

        /// 只入队（不录制）：同帧多层要在一次提交里聚合录制。
        fn queue(
            &mut self,
            kind: AtlasKind,
            layer: u16,
            spec: AtlasArraySpec,
            pixels: Vec<u8>,
            dirty: DirtyRects,
        ) {
            let queued_page = self.atlas.page(kind);
            self.uploads
                .queue_upload(kind, layer, spec, queued_page, pixels, dirty);
        }

        /// array 对齐（重建到待上传规格）+ staging 对齐 + 把当前全部待上传层录进一次立即提交。
        fn flush(&mut self) -> Vec<crate::render::retire::GpuResourceHandle> {
            let mut rebuild = self
                .uploads
                .reconcile_pages([
                    self.atlas.spec(AtlasKind::Glyph),
                    self.atlas.spec(AtlasKind::Image),
                ])
                .expect("reconcile pages");
            for request in rebuild.drain(..) {
                let _old = self
                    .atlas
                    .recreate(self.shared.raw(), request.kind, request.spec)
                    .expect("recreate atlas array");
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

        /// 入队 + 录制一次（单层便捷入口）。
        fn upload(
            &mut self,
            kind: AtlasKind,
            layer: u16,
            spec: AtlasArraySpec,
            pixels: Vec<u8>,
            dirty: DirtyRects,
        ) -> Vec<crate::render::retire::GpuResourceHandle> {
            self.queue(kind, layer, spec, pixels, dirty);
            self.flush()
        }

        /// 把一张图集某一层的整页读回 CPU（RGBA8；R8 覆盖率读回为 R = 覆盖率、A = 255）。
        fn readback(&mut self, kind: AtlasKind, page: u32, layer: u16) -> ReadbackImage {
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
                    layer,
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
            0,
            spec(TEST_PAGE, 1),
            TEST_PAGE,
            vec![0xAB; 2 * TEST_PAGE as usize],
            rows(4, 6),
        );
        assert!(
            uploads
                .reconcile_pages([spec(TEST_PAGE * 8, 1), spec(1, 1)])
                .expect("reconcile pages")
                .is_empty(),
            "非整页的旧页增量不应触发重建"
        );
        assert!(
            !uploads.pending(AtlasKind::Glyph),
            "过期增量必须在页对齐时被丢弃"
        );

        // 占位页是 1×1：整页上传代表 App 换代（入队页 1 ≠ 新页），必须要求重建。
        uploads.queue_upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            1,
            vec![0u8; (TEST_PAGE * TEST_PAGE) as usize],
            full_rects(),
        );
        assert_eq!(
            uploads
                .reconcile_pages([spec(TEST_PAGE * 8, 1), spec(1, 1)])
                .expect("reconcile pages"),
            vec![request(AtlasKind::Glyph, TEST_PAGE, 1)],
            "整页上传必须要求把共享 array 重建到它自己的页尺寸"
        );
        assert!(
            uploads
                .reconcile_pages([spec(TEST_PAGE, 1), spec(1, 1)])
                .expect("reconcile pages")
                .is_empty(),
            "页已对齐时不再要求重建"
        );
        assert!(uploads.pending(AtlasKind::Glyph));
    }

    /// 同一 extent 的多层整页上传只产生**一次**重建请求，且按**最大的层数**建数组
    /// （图片图集的层数 = App 打开的页数）。
    #[test]
    fn reconcile_pages_deduplicates_the_rebuild_request_for_every_layer() {
        let mut uploads = AtlasUploads::new();
        for layer in 0..4u16 {
            uploads.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 4),
                1,
                vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
                full_rects(),
            );
        }
        assert_eq!(
            uploads
                .reconcile_pages([spec(1, 1), spec(1, 1)])
                .expect("reconcile pages"),
            vec![request(AtlasKind::Image, TEST_PAGE, 4)],
            "按 extent 去重：数组只有一次重建，且层数取最大值"
        );
        assert!(uploads.pending(AtlasKind::Image));
        assert!(
            (0..4).all(|layer| {
                let kind = AtlasKind::Image;
                uploads.pending(kind) && uploads.pending[kind.index() as usize][layer].is_some()
            }),
            "四层都要保留到重建之后上传"
        );
    }

    /// 旧页整页上传（入队时共享页就是它，之后被别的窗口换代）不得把共享纹理重建回旧页：
    /// 否则所有窗口的 tile 都会对一个尺寸错误的纹理采样。
    #[test]
    fn reconcile_pages_discards_a_full_page_upload_of_a_superseded_page() {
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            0,
            spec(512, 1),
            512,
            vec![0u8; 512 * 512],
            full_rects(),
        );
        assert!(
            uploads
                .reconcile_pages([spec(1024, 1), spec(1, 1)])
                .expect("reconcile pages")
                .is_empty(),
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
            0,
            spec(PAGE, 1),
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
            0,
            spec(TEST_PAGE, 1),
            vec![0u8; (TEST_PAGE * TEST_PAGE) as usize],
            full_rects(),
        );
        // 增量：只有 1 行（= 页宽字节）的载荷。
        let page = vec![0xABu8; TEST_PAGE as usize];
        harness.upload(AtlasKind::Glyph, 0, spec(TEST_PAGE, 1), page, rows(5, 6));

        let image = harness.readback(AtlasKind::Glyph, TEST_PAGE, 0);
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
            0,
            spec(TEST_PAGE, 1),
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
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            vec![0xCDu8; 88],
            glyph,
        );

        let image = harness.readback(AtlasKind::Glyph, TEST_PAGE, 0);
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
            0,
            spec(TEST_PAGE, 1),
            TEST_PAGE,
            vec![0x7Fu8; 2 * TEST_PAGE as usize],
            rows(4, 6),
        );
        assert!(uploads.pending(AtlasKind::Glyph));
        assert!(!uploads.pending(AtlasKind::Image));
    }

    /// 懒创建 + 动态层数（P2-B3）：图片 array 初始是 **1 层的 1×1 占位**，有 1 页时按
    /// 「1 页 = 1 层」建成单层数组 —— 不再是「一旦有图就 4 层」的常驻 4 × 16 MiB。
    ///
    /// 变异：`layers()` 恢复成常量 `MAX_IMAGE_PAGES` ⇒ 占位/一页都是 4 层 ⇒ 本测试失败。
    #[test]
    fn the_image_array_is_lazy_and_starts_with_one_layer() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        /// 图片图集纹理在 D3D12 资源描述里的**实际**字节数（`Width × Height × 数组长度 × 4 B`）。
        ///
        /// 这是实测而不是按常量推算：`GetDesc` 返回的就是创建出来的资源尺寸。
        fn array_bytes(harness: &Harness) -> u64 {
            // SAFETY: 资源由 `SharedAtlas` 持有且存活；`GetDesc` 只读。
            let desc = unsafe { harness.atlas.texture(AtlasKind::Image).GetDesc() };
            assert_eq!(
                u64::from(desc.Height),
                desc.Width,
                "图集纹理必须是正方形（页边长一致）"
            );
            desc.Width * u64::from(desc.Height) * u64::from(desc.DepthOrArraySize) * 4
        }
        let array_layers = |harness: &Harness| {
            // SAFETY: 资源由 `SharedAtlas` 持有且存活；`GetDesc` 只读。
            unsafe { harness.atlas.texture(AtlasKind::Image).GetDesc() }.DepthOrArraySize
        };

        assert_eq!(harness.atlas.page(AtlasKind::Image), 1, "占位页是 1×1");
        assert_eq!(
            harness.atlas.layers(AtlasKind::Image),
            1,
            "占位数组只有 1 层"
        );
        assert_eq!(array_layers(&harness), 1, "占位纹理是 1×1×1 数组");
        // 占位数组 = 1×1 纹素 × BGRA8 × 1 层 = 4 B（旧实现的常量层数会建 4 层）。
        assert_eq!(array_bytes(&harness), 4, "占位数组只占 4 B");
        assert!(
            array_bytes(&harness) < 16 * 1024 * 1024,
            "没有图片时图片图集不得占 16 MiB"
        );

        // 第一页（TEST_PAGE²）：按 1 层建数组。
        harness.upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            vec![0x11u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
            full_rects(),
        );
        assert_eq!(harness.atlas.page(AtlasKind::Image), TEST_PAGE);
        assert_eq!(harness.atlas.layers(AtlasKind::Image), 1, "1 页 ⇒ 1 层");
        assert_eq!(array_layers(&harness), 1, "1 页的纹理数组必须是 1 层");
        // 实测：TEST_PAGE² × BGRA8 × 1 层（旧实现会按常量建成 4 层 = 4 倍）。
        assert_eq!(
            array_bytes(&harness),
            u64::from(TEST_PAGE) * u64::from(TEST_PAGE) * 4,
            "1 页只建 1 层（不是 4 层）"
        );
        let single_layer_bytes = array_bytes(&harness);
        assert!(
            single_layer_bytes < 64 * 1024 * 1024,
            "单层 {single_layer_bytes} B ≪ 旧实现的 4 层 64 MiB"
        );
    }

    /// 页数 1 → 2（本任务的核心语义）：数组从 1 层重建为 **2 层**，且第 0 层内容仍在。
    ///
    /// 扩层帧 App 对每个已打开页整页重传（`App::upload_image_page` 的 `array_missing` 分支），
    /// 因此重建后的数组必须有完整的两个层 —— **增长不能丢已上传的图**。
    ///
    /// 变异：`reconcile_pages` 不识别「`spec.layers > current.layers`」⇒ 不重建 ⇒ 数组仍 1 层
    /// （写 layer 1 还会越界）⇒ 本测试的层数断言失败。
    #[test]
    fn growing_the_image_array_keeps_the_existing_layer_content() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        const LAYER_ZERO: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];
        const LAYER_ONE: [u8; 4] = [0x44, 0x55, 0x66, 0xFF];
        fn fill(colour: [u8; 4]) -> Vec<u8> {
            let mut pixels = vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize];
            for texel in pixels.as_chunks_mut::<4>().0 {
                *texel = colour;
            }
            pixels
        }
        // 读回是 RGBA8：BGRA8 上传的 R/B 交换。
        fn first_texel(harness: &mut Harness, layer: u16) -> [u8; 4] {
            let image = harness.readback(AtlasKind::Image, TEST_PAGE, layer);
            image.rgba8[..4].try_into().expect("4 bytes")
        }
        const ZERO_RGBA: [u8; 4] = [LAYER_ZERO[2], LAYER_ZERO[1], LAYER_ZERO[0], LAYER_ZERO[3]];
        const ONE_RGBA: [u8; 4] = [LAYER_ONE[2], LAYER_ONE[1], LAYER_ONE[0], LAYER_ONE[3]];

        // 第 1 页：1 层数组，layer 0 = 颜色 A。
        harness.upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            fill(LAYER_ZERO),
            full_rects(),
        );
        assert_eq!(harness.atlas.layers(AtlasKind::Image), 1);
        assert_eq!(first_texel(&mut harness, 0), ZERO_RGBA);

        // 开第 2 页：扩层到 2 层，同一帧把**两个已打开页**整页重传（App 的扩层帧就是这样）。
        harness.queue(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 2),
            fill(LAYER_ZERO),
            full_rects(),
        );
        harness.queue(
            AtlasKind::Image,
            1,
            spec(TEST_PAGE, 2),
            fill(LAYER_ONE),
            full_rects(),
        );
        harness.flush();

        assert_eq!(
            harness.atlas.layers(AtlasKind::Image),
            2,
            "页数 1 → 2 ⇒ texture array 必须重建为 2 层"
        );
        // SAFETY: 资源由 `SharedAtlas` 持有且存活；`GetDesc` 只读。
        let desc = unsafe { harness.atlas.texture(AtlasKind::Image).GetDesc() };
        assert_eq!(desc.DepthOrArraySize, 2, "重建后的纹理是 2 层数组");
        // 实测资源字节数：TEST_PAGE² × BGRA8 × 2 层。
        assert_eq!(
            desc.Width * u64::from(desc.Height) * u64::from(desc.DepthOrArraySize) * 4,
            u64::from(TEST_PAGE) * u64::from(TEST_PAGE) * 4 * 2,
            "2 层数组的字节数 = 2 × 单层"
        );
        assert_eq!(
            first_texel(&mut harness, 0),
            ZERO_RGBA,
            "重建后第 0 层内容必须仍在（增长不能丢已上传的图）"
        );
        assert_eq!(
            first_texel(&mut harness, 1),
            ONE_RGBA,
            "第 1 层（新页）内容必须正确"
        );
    }

    /// 图片图集是 texture array，`CopyTextureRegion` 的 subresource = 层号；数组层数由 App 的
    /// 待上传规格决定（这里一帧请求 4 层 ⇒ 建成 4 层，而不是恒 4 层的常量）。
    ///
    /// 每层写不同的颜色并逐层读回：证明 ① 纹理数组层数等于请求的层数；② 层号进了目标
    /// subresource；③ 同一帧的多层上传共用一块 staging 时不会互相覆盖（各层按各自偏移落位）。
    #[test]
    fn image_atlas_layers_upload_to_their_own_array_slices() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        const COLORS: [[u8; 4]; 4] = [
            [0x11, 0x22, 0x33, 0xFF],
            [0x44, 0x55, 0x66, 0xFF],
            [0x77, 0x88, 0x99, 0xFF],
            [0xAA, 0xBB, 0xCC, 0xFF],
        ];
        for (layer, color) in COLORS.iter().enumerate() {
            let mut pixels = vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize];
            for texel in pixels.as_chunks_mut::<4>().0 {
                *texel = *color;
            }
            harness.queue(
                AtlasKind::Image,
                layer as u16,
                spec(TEST_PAGE, 4),
                pixels,
                full_rects(),
            );
        }
        harness.flush();
        assert_eq!(
            harness.atlas.layers(AtlasKind::Image),
            4,
            "请求 4 层 ⇒ texture array 必须建成 4 层"
        );

        for (layer, color) in COLORS.iter().enumerate() {
            let image = harness.readback(AtlasKind::Image, TEST_PAGE, layer as u16);
            assert_eq!(
                [
                    image.rgba8[0],
                    image.rgba8[1],
                    image.rgba8[2],
                    image.rgba8[3]
                ],
                [color[2], color[1], color[0], color[3]],
                "第 {layer} 层的首像素必须来自该层的上传（BGRA→RGBA）"
            );
            let last = ((TEST_PAGE * TEST_PAGE - 1) * 4) as usize;
            assert_eq!(
                [
                    image.rgba8[last],
                    image.rgba8[last + 1],
                    image.rgba8[last + 2],
                    image.rgba8[last + 3]
                ],
                [color[2], color[1], color[0], color[3]],
                "第 {layer} 层的末像素必须来自该层的上传"
            );
        }
    }

    /// 越界层号必须被**动态**层数硬断言拦住：数组只有 2 层时，layer 2 越界。
    ///
    /// 变异：断言改回常量上界（`layer < MAX_IMAGE_PAGES`）⇒ layer 2 通过 ⇒ 本测试（应 panic）
    /// 不 panic 而失败。
    #[test]
    #[should_panic(expected = "atlas layer 2 out of range")]
    fn queueing_an_out_of_range_image_layer_is_a_contract_violation() {
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Image,
            2,
            spec(TEST_PAGE, 2),
            TEST_PAGE,
            vec![0u8; 4],
            rows(0, 1),
        );
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
        harness.upload(AtlasKind::Glyph, 0, spec(BIG_PAGE, 1), glyph, full_rects());

        // 槽 1：图片整页（512 行 × 4 B；行 pitch 2048）。
        let mut image = vec![0u8; (BIG_PAGE * BIG_PAGE * 4) as usize];
        for texel in image.as_chunks_mut::<4>().0 {
            *texel = [0x33, 0x44, 0x55, 0xFF];
        }
        harness.upload(AtlasKind::Image, 0, spec(BIG_PAGE, 1), image, full_rects());

        // 两个独立的读回器：R8 与 BGRA8 的行 pitch 不同（`Readback` 按尺寸/格式缓存）。
        let glyph_image = harness.readback(AtlasKind::Glyph, BIG_PAGE, 0);
        let glyph_row = |row: u32| glyph_image.rgba8[(row * BIG_PAGE * 4) as usize];
        assert_eq!(glyph_row(0), 0x11, "glyph row 0");
        assert_eq!(glyph_row(BIG_PAGE - 1), 0x11, "glyph last row");

        let image_image = harness.readback(AtlasKind::Image, BIG_PAGE, 0);
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
            0,
            spec(64, 1),
            vec![0u8; (64 * 64) as usize],
            full_rects(),
        );
        let live = harness.shared.live_resources();
        assert_eq!(live, MAX_FRAMES_IN_FLIGHT, "页 64 只注册 3 个常驻 staging");

        // 页 128：整页上传 = App 换代 ⇒ 重建共享页 + 整组 staging 换代。
        let retired = harness.upload(
            AtlasKind::Glyph,
            0,
            spec(128, 1),
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
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            page.clone(),
            full_rects(),
        );
        let snapshot = harness.atlas.shadow_snapshot();
        let shadow = snapshot[0]
            .as_ref()
            .expect("glyph shadow after full upload");
        assert_eq!(shadow.page, TEST_PAGE);
        assert_eq!(
            shadow.layers[0].as_deref(),
            Some(page.as_slice()),
            "影子页必须与整页字节 1:1"
        );

        // 增量上传：只有脏行覆盖，其余像素保持（打包缓冲里只有脏行）。
        let dirty_row = vec![0x77u8; row_bytes];
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            dirty_row,
            rows(5, 6),
        );
        let snapshot = harness.atlas.shadow_snapshot();
        let bytes = snapshot[0].as_ref().expect("glyph shadow").layers[0]
            .as_ref()
            .expect("glyph layer shadow");
        assert_eq!(bytes[5 * row_bytes + 3], 0x77, "脏行被覆盖");
        assert_eq!(bytes[4 * row_bytes], 0, "脏行之外保持不变");

        // AOI 增量（R06-3）：4×2 的矩形只覆盖自己的列/行，影子页的其它字节保持原样。
        let aoi = rect(3, 7, 7, 9);
        assert_eq!(aoi.byte_count(TEST_PAGE, 1), 8);
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            vec![0x99u8; 8],
            aoi,
        );
        let snapshot = harness.atlas.shadow_snapshot();
        let bytes = snapshot[0].as_ref().expect("glyph shadow").layers[0]
            .as_ref()
            .expect("glyph layer shadow");
        assert_eq!(bytes[7 * row_bytes + 3], 0x99, "AOI 左上角");
        assert_eq!(bytes[8 * row_bytes + 6], 0x99, "AOI 右下角");
        assert_eq!(bytes[7 * row_bytes + 2], 0, "AOI 左侧一列保持原样");
        assert_eq!(bytes[7 * row_bytes + 7], 0, "AOI 右侧一列保持原样");
        assert_eq!(bytes[6 * row_bytes + 3], 0, "AOI 上一行保持原样");
        assert_eq!(bytes[9 * row_bytes + 3], 0, "AOI 下一行保持原样");

        // 页换代：影子页按新页整页重建。
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(128, 1),
            vec![0x11u8; 128 * 128],
            full_rects(),
        );
        let snapshot = harness.atlas.shadow_snapshot();
        let shadow = snapshot[0]
            .as_ref()
            .expect("glyph shadow after page change");
        assert_eq!(shadow.page, 128);
        let bytes = shadow.layers[0].as_ref().expect("glyph layer shadow");
        assert_eq!(bytes.len(), 128 * 128);
        assert!(bytes.iter().all(|byte| *byte == 0x11));
    }

    /// 图片图集的影子按**层**保存（设备恢复要逐层重传）；未上传过的层是 `None`。
    /// 影子层数跟随数组的运行时层数（这里请求 3 层），不是 `MAX_IMAGE_PAGES` 常量。
    #[test]
    fn the_image_atlas_shadow_is_tracked_per_layer() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        // 数组先按「App 已完成完整扩层」建到 3 层（重建本身要求所有层整页上传，见
        // `reconcile_pages` 的 fail-closed 判据）；本测试只验证**影子内容**逐层独立：
        // 只上传 layer 0 / 2，layer 1 应当没有影子。
        let _grown = harness
            .atlas
            .recreate(harness.shared.raw(), AtlasKind::Image, spec(TEST_PAGE, 3))
            .expect("grow image array to 3 layers");
        harness.queue(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 3),
            vec![0x11u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
            full_rects(),
        );
        harness.queue(
            AtlasKind::Image,
            2,
            spec(TEST_PAGE, 3),
            vec![0x22u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
            full_rects(),
        );
        harness.flush();
        let snapshot = harness.atlas.shadow_snapshot();
        let shadow = snapshot[1].as_ref().expect("image shadow");
        assert_eq!(shadow.page, TEST_PAGE);
        assert_eq!(
            shadow.layers.len(),
            3,
            "影子层数 = 数组当前层数（请求 3 层）"
        );
        assert!(
            shadow.layers[0]
                .as_ref()
                .is_some_and(|bytes| bytes.iter().all(|byte| *byte == 0x11))
        );
        assert!(shadow.layers[1].is_none(), "未上传的层没有影子");
        assert!(
            shadow.layers[2]
                .as_ref()
                .is_some_and(|bytes| bytes.iter().all(|byte| *byte == 0x22))
        );
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
            0,
            spec(BIG_PAGE, 1),
            vec![0u8; (BIG_PAGE * BIG_PAGE) as usize],
            full_rects(),
        );

        // 打包布局：矩形按行号顺序紧密排列（10 行 0x44 + 20 行 0x55，都是整页宽）。
        let mut page = vec![0x44u8; (10 * BIG_PAGE) as usize];
        page.extend(std::iter::repeat_n(0x55u8, (20 * BIG_PAGE) as usize));
        let mut dirty = DirtyRects::default();
        dirty.mark(PhysicalRect::new(0, 10, i32::MAX, 20));
        dirty.mark(PhysicalRect::new(0, 300, i32::MAX, 320));
        harness.upload(AtlasKind::Glyph, 0, spec(BIG_PAGE, 1), page, dirty);

        let image = harness.readback(AtlasKind::Glyph, BIG_PAGE, 0);
        let row = |row: u32| image.rgba8[(row * BIG_PAGE * 4) as usize];
        assert_eq!(row(0), 0, "untouched row before the first rect");
        assert_eq!(row(15), 0x44, "first rect content");
        assert_eq!(row(100), 0, "gap between the rects stays untouched");
        assert_eq!(row(299), 0, "row before the second rect");
        assert_eq!(row(305), 0x55, "second rect content");
        assert_eq!(row(511), 0, "untouched row after the last rect");
    }

    /// fail-closed（RBT-01 不变量 2）：扩层重建要求**每一层都有整页上传**。缺层、或某层只有
    /// 增量矩形，都必须**拒绝**（release 也生效，不是 `debug_assert`）——重建后的数组会有
    /// 半成品层，一旦被 draw 采样就是未定义内容。
    #[test]
    fn reconcile_pages_rejects_growth_without_a_full_page_upload_for_every_layer() {
        // 情形 1：缺层（layer 1 完全没有待上传）。
        let mut uploads = AtlasUploads::new();
        for layer in [0u16, 2] {
            uploads.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 3),
                1,
                vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
                full_rects(),
            );
        }
        let error = uploads
            .reconcile_pages([spec(TEST_PAGE, 3), spec(1, 1)])
            .expect_err("缺层的扩层请求必须被拒绝");
        assert!(
            matches!(
                error,
                crate::error::EngineError::AtlasGrowthIncomplete {
                    kind: AtlasKind::Image,
                    layer: 1,
                    layers: 3,
                }
            ),
            "unexpected error: {error:?}"
        );

        // 情形 2：层在，但只带增量矩形（不是整页）。
        let mut uploads = AtlasUploads::new();
        for layer in 0..3u16 {
            let rects = if layer == 1 { rows(2, 4) } else { full_rects() };
            let bytes = rects.byte_count(TEST_PAGE, 4);
            uploads.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 3),
                1,
                vec![0u8; bytes],
                rects,
            );
        }
        let error = uploads
            .reconcile_pages([spec(TEST_PAGE, 3), spec(1, 1)])
            .expect_err("非整页的扩层请求必须被拒绝");
        assert!(
            matches!(
                error,
                crate::error::EngineError::AtlasGrowthIncomplete {
                    kind: AtlasKind::Image,
                    layer: 1,
                    layers: 3,
                }
            ),
            "unexpected error: {error:?}"
        );
    }

    /// 反例（正路径）：每层都整页 ⇒ 接受，并按最大层数去重成一条请求。
    #[test]
    fn reconcile_pages_accepts_growth_when_every_layer_is_a_full_page() {
        let mut uploads = AtlasUploads::new();
        for layer in 0..3u16 {
            uploads.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 3),
                1,
                vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
                full_rects(),
            );
        }
        assert_eq!(
            uploads
                .reconcile_pages([spec(TEST_PAGE, 3), spec(1, 1)])
                .expect("全层整页的扩层请求必须被接受"),
            vec![request(AtlasKind::Image, TEST_PAGE, 3)],
            "一次重建、层数取最大值"
        );
    }
}

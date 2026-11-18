//! 共享图集纹理（[`SharedAtlas`]）+ 每窗口上传器（[`AtlasUploads`]）
//! （P1-2.9）。
//!
//! 字形图集是 `R8_UNORM`（线性覆盖度），图片图集是 `B8G8R8A8_SRGB`。两张纹理是**进程级
//! 共享资源**，由 `device::SharedGpu` 持有；每窗口只有自己的**上传路径**：
//!
//! - [`SharedAtlas`]：两张纹理 + 一个 sampler + 当前页尺寸。懒分配：初始化只建 1×1 占位页
//!   （描述符集合里始终有有效视图），第一次整页上传 / 页档换代时由
//!   `SharedGpu::rebuild_atlas_pages` 重建。新页在写锁内先转到 `SHADER_READ_ONLY_OPTIMAL`，
//!   因此所有窗口的增量上传都走同一条 `SHADER_READ_ONLY → TRANSFER_DST → SHADER_READ_ONLY`
//!   屏障序列 —— 不需要「哪张页初始化过」的可变状态（那是跨窗口竞态）。
//! - [`AtlasUploads`]（每窗口）：待上传页 + 每 in-flight 槽 × 每图集的 staging。staging 必须
//!   是每窗口的：同一份共享纹理会被多个窗口的 GPU 线程并发上传，各窗口的 staging 不能互相
//!   覆盖；每个槽的 staging 又只在本窗口该槽的 fence 完成前复用（与 `UploadArena` 同一套
//!   in-flight 语义）。
//!
//! 并发模型（与 Windows 后端的 `SharedAtlas` / `AtlasUploads` 同构）：窗口的 GPU 线程在
//! 图集**读锁**内录制并提交一帧（页在录制期间不会变），页重建走**写锁**并等待共享队列 idle
//! （旧纹理 / 旧描述符不再被任何在飞命令引用）。同一条队列上的命令按提交顺序执行，因此多个
//! 窗口同时上传同一纹理是安全的：每个命令缓冲内部的屏障序列不会交错。
//!
//! staging 有两档：**每槽常驻**一小块（≤ 1 MiB，增量上传复用）与**一次性**大块（整页上传，
//! 录制后按帧 retire）。常驻容量因此不随图集页尺寸放大（DEVIATIONS F-216）。页尺寸随上传
//! **显式过线**（S-153），不由 `pixels.len()` 反推：增量载荷只有脏矩形，长度不足以恢复整页尺寸。

use ash::vk;

use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        atlas::{DirtyRects, MAX_RESIDENT_STAGING_BYTES, RectChunk, texel_bytes},
        frame::MAX_FRAMES_IN_FLIGHT,
        linux::{
            device::{Device, GpuObject, SharedGpu, init_error},
            memory::MemoryNeed,
        },
        retire::GpuResourceHandle,
    },
};

/// 初始化时的占位页（1×1）：让描述符集合里的图集视图在第一次上传之前也有效。
const PLACEHOLDER_PAGE: u32 = 1;

/// 一张共享图集页（image + memory + view + 页尺寸）。
struct SharedPage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    page: u32,
}

impl SharedPage {
    fn new(device: &Device, kind: AtlasKind, page: u32) -> Result<Self, EngineError> {
        let format = texture_format(kind);
        // 产品路径只采样 / 上传，不读回图集；单测要验证「两帧各自的 staging 落到了自己的行」，
        // 因此 `cfg(test)` 下额外允许 `TRANSFER_SRC`（usage 是创建参数，测试无法事后追加）。
        let usage = vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST;
        #[cfg(test)]
        let usage = usage | vk::ImageUsageFlags::TRANSFER_SRC;
        // SAFETY: 句柄归调用方独占；失败路径就地销毁，成功路径交给 `SharedAtlas`。
        let image = unsafe {
            device.raw().create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width: page,
                        height: page,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .map_err(|error| init_error("create_image(atlas)", format!("{error:?}")))?;
        // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
        let requirements = unsafe { device.raw().get_image_memory_requirements(image) };
        let Some(memory_type) =
            device.memory_type(requirements.memory_type_bits, MemoryNeed::DEVICE_IMAGE)
        else {
            // SAFETY: image 已创建且尚未交给任何人。
            unsafe { device.raw().destroy_image(image, None) };
            return Err(init_error(
                "atlas image memory",
                "no device-local memory type",
            ));
        };
        // SAFETY: `requirements` 来自本 image；`memory_type` 由 memory_type 查询保证合法。
        let memory = match unsafe {
            device.raw().allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type),
                None,
            )
        } {
            Ok(memory) => memory,
            Err(error) => {
                // SAFETY: image 已创建且尚未交给任何人。
                unsafe { device.raw().destroy_image(image, None) };
                return Err(init_error("allocate_memory(atlas)", format!("{error:?}")));
            }
        };
        // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定/写入的范围在其分配内。
        if let Err(error) = unsafe { device.raw().bind_image_memory(image, memory, 0) } {
            // SAFETY: 两个句柄都还没有交给任何人。
            unsafe {
                device.raw().destroy_image(image, None);
                device.raw().free_memory(memory, None);
            }
            return Err(init_error("bind_image_memory(atlas)", format!("{error:?}")));
        }
        // SAFETY: 句柄与参数都由调用方独占，调用期间有效。
        let view = match unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        } {
            Ok(view) => view,
            Err(error) => {
                // SAFETY: 三个句柄都还没有交给任何人。
                unsafe {
                    device.raw().destroy_image(image, None);
                    device.raw().free_memory(memory, None);
                }
                return Err(init_error("create_image_view(atlas)", format!("{error:?}")));
            }
        };
        Ok(Self {
            image,
            memory,
            view,
            page,
        })
    }

    /// 销毁页的全部句柄（Vulkan 要求先销毁 view、再 image、最后释放内存）。
    fn destroy(&self, device: &ash::Device) {
        // SAFETY: 句柄归本结构独占；调用方保证 GPU 不再引用它。
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// 进程级共享的两张图集纹理（由 `SharedGpu` 的图集读写锁保护）。
pub(crate) struct SharedAtlas {
    device: ash::Device,
    glyph: SharedPage,
    image: SharedPage,
    /// 两张页共用同一个 sampler（参数逐字相同，合并成一个句柄）。
    sampler: vk::Sampler,
}

impl SharedAtlas {
    /// 建两张 1×1 占位页 + sampler；真实页由 `SharedGpu::rebuild_atlas_pages` 生效。
    pub(crate) fn new(device: &Device) -> Result<Self, EngineError> {
        let glyph = SharedPage::new(device, AtlasKind::Glyph, PLACEHOLDER_PAGE)?;
        let image = match SharedPage::new(device, AtlasKind::Image, PLACEHOLDER_PAGE) {
            Ok(page) => page,
            Err(error) => {
                glyph.destroy(device.raw());
                return Err(error);
            }
        };
        // SAFETY: 句柄归本结构独占；`Drop` 里销毁。
        let sampler = match unsafe {
            device.raw().create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .min_lod(0.0)
                    .max_lod(0.0),
                None,
            )
        } {
            Ok(sampler) => sampler,
            Err(error) => {
                glyph.destroy(device.raw());
                image.destroy(device.raw());
                return Err(init_error("create_sampler(atlas)", format!("{error:?}")));
            }
        };
        Ok(Self {
            device: device.raw().clone(),
            glyph,
            image,
            sampler,
        })
    }

    pub(crate) fn glyph_view(&self) -> vk::ImageView {
        self.glyph.view
    }

    pub(crate) fn image_view(&self) -> vk::ImageView {
        self.image.view
    }

    pub(crate) fn sampler(&self) -> vk::Sampler {
        self.sampler
    }

    /// 该图集的 GPU image（每窗口上传的 copy 目标）。
    pub(crate) fn image(&self, kind: AtlasKind) -> vk::Image {
        self.page_ref(kind).image
    }

    /// 该图集当前的页尺寸（纹素，边长）。
    pub(crate) fn page(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).page
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

    /// 换页：建新 image / memory / view 并替换（调用方已等共享队列 idle，旧句柄立即销毁）。
    pub(crate) fn recreate(
        &mut self,
        device: &Device,
        kind: AtlasKind,
        page: u32,
    ) -> Result<(), EngineError> {
        let next = SharedPage::new(device, kind, page)?;
        let old = std::mem::replace(self.page_mut(kind), next);
        old.destroy(&self.device);
        Ok(())
    }

    /// 录制「新建页从 `UNDEFINED` 转到 `SHADER_READ_ONLY_OPTIMAL`」的屏障。
    ///
    /// 只对 `kinds` 里的页发屏障（占位页初始化 / 每次重建的新页）。
    pub(crate) fn record_layout_transitions(
        &self,
        device: &ash::Device,
        command: vk::CommandBuffer,
        kinds: &[AtlasKind],
    ) {
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barriers: Vec<vk::ImageMemoryBarrier> = kinds
            .iter()
            .map(|kind| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .image(self.image(*kind))
                    .subresource_range(range)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
            })
            .collect();
        if barriers.is_empty() {
            return;
        }
        // SAFETY: 屏障目标都是本结构独占的 image，范围在图像边界内。
        unsafe {
            device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
        }
    }
}

impl Drop for SharedAtlas {
    fn drop(&mut self) {
        // SAFETY: 句柄归本结构独占；`SharedGpu` 保证设备在本结构 drop 之后销毁。
        unsafe {
            self.device.destroy_sampler(self.sampler, None);
        }
        self.glyph.destroy(&self.device);
        self.image.destroy(&self.device);
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
    /// `rects` 顺序逐矩形紧密打包的字节（整页上传时就是整页）。
    pixels: Vec<u8>,
    rects: DirtyRects,
}

impl PendingUpload {
    /// 是否覆盖整页（首次上传 / 页换代之后的唯一合法载荷）。
    fn covers_full_page(&self) -> bool {
        self.rects.covers_page(self.page)
    }
}

/// 每窗口的图集上传路径：待上传页 + 每 in-flight 槽 × 每图集的 staging。
pub(crate) struct AtlasUploads {
    pending: [Option<PendingUpload>; 2],
    staging: [StagingSet; 2],
}

impl AtlasUploads {
    pub(crate) const fn new() -> Self {
        Self {
            pending: [None, None],
            staging: [StagingSet::new(), StagingSet::new()],
        }
    }

    /// 该图集是否有一笔上传已入队、还没被命令缓冲消费（离屏目标的空闲判据）。
    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        self.pending[kind.index() as usize].is_some()
    }

    /// 入队一页的脏矩形（`pixels` 是 `rects` 顺序**逐矩形紧密打包**的字节，R06-3）。
    ///
    /// `pixels` 按**所有权**移动进待上传槽，避免槽 → 纹理之间再复制一次（2048² 图片页是
    /// 16 MiB）。长度必须等于 [`DirtyRects::byte_count`]（release 也硬断言：越界读会读穿堆）。
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
    /// 直接丢弃 —— 重建回旧页会让所有窗口对错误尺寸的纹理采样，且丢弃的行会被下一次整页
    /// 上传（页内容是进程级的）覆盖。
    ///
    /// 调用方在 `SharedGpu` 的图集读锁内调用；返回非空时先释放读锁，
    /// `SharedGpu::rebuild_atlas_pages` 在写锁内落地新页，然后重试。
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

    /// 在本帧命令缓冲里录制全部待上传页（每页 = staging 写入 + 逐区间 `cmd_copy_buffer_to_image`）。
    ///
    /// 返回本帧扩容 / 一次性 staging 换下的句柄（调用方在本帧录制结束后 retire；旧的常驻
    /// staging 在本帧槽的 fence 已完成后立即销毁是安全的，这里直接在 `StagingSet` 内 drop）。
    pub(crate) fn record_uploads(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        slot: usize,
        command: vk::CommandBuffer,
    ) -> Result<Vec<GpuResourceHandle>, EngineError> {
        let mut retired = Vec::new();
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let kind_index = kind.index() as usize;
            let Some(pending) = self.pending[kind_index].take() else {
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
            let image = atlas.image(kind);
            self.staging[kind_index].record(
                shared,
                kind,
                image,
                slot,
                command,
                &pending,
                &mut retired,
            )?;
        }
        Ok(retired)
    }
}

/// 一个图集在每 in-flight 槽上的常驻 staging。
struct StagingSet {
    slots: [Option<StagingRing>; MAX_FRAMES_IN_FLIGHT],
}

impl StagingSet {
    const fn new() -> Self {
        Self {
            slots: [None, None, None],
        }
    }

    /// 录制一页的上传：staging 写入 + 屏障 + 逐矩形 `cmd_copy_buffer_to_image` + 屏障。
    ///
    /// AOI（R06-3）：每个脏矩形单独一次 `vkCmdCopyBufferToImage`，`image_offset` 与
    /// `image_extent` 都取矩形自己的 `(x, y, w, h)`（缺 `image_offset` 会从第 0 行覆写整页；
    /// 用整页宽做 `image_extent` 就是旧的放大根因）。
    fn record(
        &mut self,
        shared: &SharedGpu,
        kind: AtlasKind,
        image: vk::Image,
        slot: usize,
        command: vk::CommandBuffer,
        pending: &PendingUpload,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        let page = pending.page;
        let texel = texel_bytes(kind);
        // 打包布局（rect 顺序、行内只有 width × texel）：与该函数的 copy 顺序逐个对应。
        let chunks: Vec<RectChunk> = pending.rects.chunks(page, texel).collect();
        if chunks.is_empty() {
            // 空集合 = 零上传：不分配 staging、不录 copy。
            return Ok(());
        }
        // staging 布局：每块起点按 4 字节对齐（Vulkan 要求 bufferOffset 是纹素块大小的倍数，
        // 4 对齐对 R8/BGRA8 都成立），块内行紧密排列（`bufferRowLength = 0`）。
        let mut layout: Vec<(RectChunk, u64)> = Vec::with_capacity(chunks.len());
        let mut length = 0usize;
        for chunk in chunks {
            let offset = length.next_multiple_of(4);
            length = offset + (chunk.width * chunk.height) as usize * texel as usize;
            layout.push((chunk, offset as u64));
        }
        // 小上传复用每槽常驻 staging；整页级的大上传用一次性缓冲（本帧后 retire），
        // 常驻容量不随页尺寸放大（DEVIATIONS F-216）。
        let transient = length as u64 > MAX_RESIDENT_STAGING_BYTES;
        let mut transient_staging: Option<TransientStaging> = None;
        let (staging_buffer, staging_ptr) = if transient {
            let staging = TransientStaging::new(shared, length as u64)?;
            let handles = (staging.buffer, staging.ptr);
            transient_staging = Some(staging);
            handles
        } else {
            self.ensure(shared, slot, length)?;
            let ring = self.slots[slot]
                .as_ref()
                .expect("ensure must fill the slot");
            (ring.buffer, ring.ptr)
        };
        // 逐矩形把打包字节搬到 staging 的块起点（块间可能有 ≤3 字节的对齐填充）。
        // SAFETY: 映射在创建时常驻，`offset + 块字节数 ≤ length ≤ 容量`；`pending.pixels`
        // 的长度已由 `queue_upload` 的 `byte_count` 断言保证，`chunk.offset + 块字节数` 在界内。
        unsafe {
            for (chunk, offset) in &layout {
                let bytes = (chunk.width * chunk.height) as usize * texel as usize;
                std::ptr::copy_nonoverlapping(
                    pending.pixels.as_ptr().add(chunk.offset),
                    staging_ptr.add(*offset as usize),
                    bytes,
                );
            }
        }

        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let to_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        // SAFETY: barrier 与 copy 的目标都是共享图集页（读锁在手，页不会变）；范围在图像
        // 边界内；staging 是本次上传独占的缓冲（装全部块），每块的 offset/extent 来自
        // `DirtyRects::chunks` 的页内钳位。
        unsafe {
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            for (chunk, offset) in &layout {
                shared.raw().cmd_copy_buffer_to_image(
                    command,
                    staging_buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy::default()
                        // 每块在 staging 里各有偏移（块间 4 字节对齐）。
                        .buffer_offset(*offset)
                        // 0 = 行按 `image_extent.width` 紧密排列（块内只有 width × texel 字节）。
                        .buffer_row_length(0)
                        .buffer_image_height(0)
                        // AOI：目标从矩形自己的 (x, y) 开始。
                        .image_offset(vk::Offset3D {
                            x: chunk.x as i32,
                            y: chunk.y as i32,
                            z: 0,
                        })
                        .image_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .layer_count(1),
                        )
                        .image_extent(vk::Extent3D {
                            width: chunk.width,
                            height: chunk.height,
                            depth: 1,
                        })],
                );
            }
            let to_sampled = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_sampled],
            );
        }
        if let Some(staging) = transient_staging {
            // 本帧命令缓冲还在读它：解除映射后按帧序号 retire（buffer 先于 memory）。
            staging.retire(shared, retired);
        }
        Ok(())
    }

    /// 保证第 `slot` 个常驻 staging 至少 `bytes` 字节；扩容时旧环立即销毁（该槽上一帧已
    /// 经 `wait_slot` 证明完成，旧环不再被任何在飞命令引用）。
    fn ensure(&mut self, shared: &SharedGpu, slot: usize, bytes: usize) -> Result<(), EngineError> {
        if self.slots[slot]
            .as_ref()
            .is_some_and(|ring| ring.capacity >= bytes as u64)
        {
            return Ok(());
        }
        let ring = StagingRing::new(shared, bytes as u64)?;
        self.slots[slot] = Some(ring);
        Ok(())
    }
}

/// 每 in-flight 槽一块常驻 staging（host-visible、持久映射；DEVIATIONS F-212）。
struct StagingRing {
    device: ash::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: u64,
    /// 持久映射的写入指针（创建时 `map_memory` 一次，扩容 / 销毁时 `unmap_memory`）。
    ptr: *mut u8,
}

// SAFETY: `ptr` 只在 `kytrix-vulkan` GPU 线程上写入（窗口渲染器独占该线程）。
unsafe impl Send for StagingRing {}

impl StagingRing {
    fn new(shared: &SharedGpu, bytes: u64) -> Result<Self, EngineError> {
        let (buffer, memory, ptr) = create_host_staging(shared, bytes)?;
        Ok(Self {
            device: shared.raw().clone(),
            buffer,
            memory,
            capacity: bytes,
            ptr,
        })
    }
}

impl Drop for StagingRing {
    fn drop(&mut self) {
        // SAFETY: 映射由 `StagingRing::new` 建立，这里是唯一解除点；句柄归本结构独占，
        // 调用方保证该槽上一帧已经完成、`vkFreeMemory` 之前已解除映射。
        unsafe {
            self.device.unmap_memory(self.memory);
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// 超过常驻上限的一次性 staging（录制之后按帧序号 retire）。
struct TransientStaging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
}

impl TransientStaging {
    fn new(shared: &SharedGpu, bytes: u64) -> Result<Self, EngineError> {
        let (buffer, memory, ptr) = create_host_staging(shared, bytes)?;
        Ok(Self {
            buffer,
            memory,
            ptr,
        })
    }

    /// 解除映射并把 buffer / memory 登记进共享句柄表，等本帧完成之后真正释放。
    fn retire(self, shared: &SharedGpu, retired: &mut Vec<GpuResourceHandle>) {
        // SAFETY: 映射由 `new` 建立；GPU 访问不受 unmap 影响。
        unsafe { shared.raw().unmap_memory(self.memory) };
        retired.push(shared.allocate(GpuObject::Buffer(self.buffer)));
        retired.push(shared.allocate(GpuObject::Memory(self.memory)));
    }
}

/// 建一块 host-visible、持久映射的 staging 缓冲。
fn create_host_staging(
    shared: &SharedGpu,
    bytes: u64,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8), EngineError> {
    // SAFETY: 句柄归调用方独占；失败路径就地销毁。
    let buffer = unsafe {
        shared.raw().create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE),
            None,
        )
    }
    .map_err(|error| init_error("create_buffer(atlas staging)", format!("{error:?}")))?;
    // SAFETY: 只读查询；句柄由调用方独占，返回值在本次调用期间有效。
    let requirements = unsafe { shared.raw().get_buffer_memory_requirements(buffer) };
    let Some(memory_type) =
        shared.memory_type(requirements.memory_type_bits, MemoryNeed::HOST_WRITABLE)
    else {
        // SAFETY: buffer 已创建且尚未交给任何人。
        unsafe { shared.raw().destroy_buffer(buffer, None) };
        return Err(init_error(
            "atlas staging memory",
            "no host-visible memory type",
        ));
    };
    // SAFETY: 句柄与参数都由本结构独占，调用期间有效。
    let memory = match unsafe {
        shared.raw().allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type),
            None,
        )
    } {
        Ok(memory) => memory,
        Err(error) => {
            // SAFETY: buffer 已创建且尚未交给任何人。
            unsafe { shared.raw().destroy_buffer(buffer, None) };
            return Err(init_error(
                "allocate_memory(atlas staging)",
                format!("{error:?}"),
            ));
        }
    };
    // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定范围在其分配内。
    if let Err(error) = unsafe { shared.raw().bind_buffer_memory(buffer, memory, 0) } {
        // SAFETY: 两个句柄都还没有交给任何人。
        unsafe {
            shared.raw().destroy_buffer(buffer, None);
            shared.raw().free_memory(memory, None);
        }
        return Err(init_error(
            "bind_buffer_memory(atlas staging)",
            format!("{error:?}"),
        ));
    }
    // SAFETY: 内存是 HOST_VISIBLE|HOST_COHERENT；映射一直持有到扩容 / 销毁 / retire。
    let ptr = unsafe {
        shared
            .raw()
            .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
    }
    .map_err(|error| {
        // SAFETY: 两个句柄都还没有交给任何人。
        unsafe {
            shared.raw().destroy_buffer(buffer, None);
            shared.raw().free_memory(memory, None);
        }
        init_error("map_memory(atlas staging)", format!("{error:?}"))
    })?;
    Ok((buffer, memory, ptr.cast::<u8>()))
}

fn texture_format(kind: AtlasKind) -> vk::Format {
    match kind {
        // 字形是线性覆盖度，**非** sRGB（P07 §2.9）。
        AtlasKind::Glyph => vk::Format::R8_UNORM,
        // 图片纹素是「linear 预乘 → sRGB 编码」，视图必须是 sRGB（contract §8）。
        AtlasKind::Image => vk::Format::B8G8R8A8_SRGB,
    }
}

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::AtlasUploads;
    use crate::{
        paint::atlas::AtlasKind,
        render::{
            atlas::DirtyRects,
            linux::{
                device::{FrameSync, GpuObject, SharedGpu, gpu_test_lock},
                memory::MemoryNeed,
            },
        },
    };

    /// 小页（16²）够验证行落位；真实页 2048² 在单测里代价过高。
    const TEST_PAGE: u32 = 16;

    /// 整页哨兵（新窗口 / 页尺寸变化的载荷）。
    fn full_rects() -> DirtyRects {
        DirtyRects::full()
    }

    /// 整页宽的脏行矩形（等价于旧的 `mark(y0, y1)` 语义）。
    fn rows(y0: u32, y1: u32) -> DirtyRects {
        let mut rects = DirtyRects::default();
        rects.mark(crate::math::PhysicalRect::new(
            0,
            y0 as i32,
            i32::MAX,
            y1 as i32,
        ));
        rects
    }

    /// 一页 BGRA8 像素（行优先；初始全 0）。
    fn page() -> Vec<u8> {
        vec![0u8; (TEST_PAGE * TEST_PAGE) as usize * 4]
    }

    fn fill_row(pixels: &mut [u8], row: u32, colour: [u8; 4]) {
        let start = row as usize * TEST_PAGE as usize * 4;
        for texel in pixels[start..start + TEST_PAGE as usize * 4]
            .as_chunks_mut::<4>()
            .0
        {
            *texel = colour;
        }
    }

    fn command_pool(gpu: &SharedGpu) -> vk::CommandPool {
        // SAFETY: 池归测试独占；结束时销毁。
        unsafe {
            gpu.raw()
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(gpu.queue_family())
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .expect("command pool")
        }
    }

    fn allocate(gpu: &SharedGpu, pool: vk::CommandPool, count: u32) -> Vec<vk::CommandBuffer> {
        // SAFETY: 池由调用方持有，命令缓冲在提交完成后释放。
        unsafe {
            gpu.raw()
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(count),
                )
                .expect("command buffers")
        }
    }

    fn begin(gpu: &SharedGpu, command: vk::CommandBuffer) {
        // SAFETY: 命令缓冲由本测试独占，提交前只录制一次。
        unsafe {
            gpu.raw()
                .reset_command_buffer(command, vk::CommandBufferResetFlags::empty())
                .expect("reset command buffer");
            gpu.raw()
                .begin_command_buffer(
                    command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .expect("begin command buffer");
        }
    }

    fn end(gpu: &SharedGpu, command: vk::CommandBuffer) {
        // SAFETY: 录制收尾；随后提交并等待。
        unsafe { gpu.raw().end_command_buffer(command) }.expect("end command buffer");
    }

    fn submit_and_wait(gpu: &SharedGpu, command: vk::CommandBuffer) {
        let commands = [command];
        // SAFETY: 命令缓冲已录制完毕；同步等待只发生在测试里。
        gpu.with_queue(|queue| unsafe {
            gpu.raw()
                .queue_submit(
                    queue,
                    &[vk::SubmitInfo::default().command_buffers(&commands)],
                    vk::Fence::null(),
                )
                .expect("queue submit");
        });
        gpu.wait_queue_idle().expect("queue idle");
    }

    /// 读回图集页某个纹素的原始存储字节（BGRA，不做通道交换）。
    fn read_texel(
        gpu: &SharedGpu,
        pool: vk::CommandPool,
        image: vk::Image,
        x: u32,
        y: u32,
    ) -> [u8; 4] {
        let bytes = 4u64;
        // SAFETY: 句柄归本测试独占；提交并等待后读回，结束时销毁。
        unsafe {
            let buffer = gpu
                .raw()
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .expect("readback buffer");
            let requirements = gpu.raw().get_buffer_memory_requirements(buffer);
            let memory_type = gpu
                .memory_type(requirements.memory_type_bits, MemoryNeed::HOST_WRITABLE)
                .expect("host-visible readback memory");
            let memory = gpu
                .raw()
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type),
                    None,
                )
                .expect("readback memory");
            gpu.raw()
                .bind_buffer_memory(buffer, memory, 0)
                .expect("bind readback memory");

            let command = allocate(gpu, pool, 1)[0];
            begin(gpu, command);
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            let to_src = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
            gpu.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
            gpu.raw().cmd_copy_image_to_buffer(
                command,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[vk::BufferImageCopy::default()
                    .buffer_offset(0)
                    .image_offset(vk::Offset3D {
                        x: x as i32,
                        y: y as i32,
                        z: 0,
                    })
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: 1,
                        height: 1,
                        depth: 1,
                    })],
            );
            let back = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            gpu.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[back],
            );
            end(gpu, command);
            submit_and_wait(gpu, command);

            let pointer = gpu
                .raw()
                .map_memory(memory, 0, bytes, vk::MemoryMapFlags::empty())
                .expect("map readback");
            let mut pixel = [0u8; 4];
            std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), pixel.as_mut_ptr(), 4);
            gpu.raw().unmap_memory(memory);
            gpu.raw().free_command_buffers(pool, &[command]);
            gpu.raw().destroy_buffer(buffer, None);
            gpu.raw().free_memory(memory, None);
            pixel
        }
    }

    /// 页对齐决策（纯函数，不建真实设备）：
    /// - 新页（整页 + 入队时共享页还不是它）⇒ 重建共享纹理；
    /// - 旧页（入队时共享页就是它，之后被别的窗口换代）⇒ 丢弃，无论整页还是增量；
    /// - 与共享页一致的增量 ⇒ 保留待录制。
    #[test]
    fn page_reconciliation_distinguishes_new_pages_from_stale_uploads() {
        // 1) App 换代的新页（整页 + 入队页 1 ≠ 页 64）：要重建共享纹理。
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            64,
            1,
            vec![0u8; (64 * 64) as usize],
            full_rects(),
        );
        assert_eq!(
            uploads.reconcile_pages([1, 1]),
            vec![(AtlasKind::Glyph, 64)]
        );
        assert!(uploads.pending(AtlasKind::Glyph), "新页要留到重建之后上传");

        // 2) 旧整页（入队时共享页就是它，之后别的窗口换代）：丢弃；不把共享纹理重建回旧页。
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            64,
            64,
            vec![0u8; (64 * 64) as usize],
            full_rects(),
        );
        assert!(uploads.reconcile_pages([128, 1]).is_empty());
        assert!(!uploads.pending(AtlasKind::Glyph), "旧整页必须丢弃");

        // 3) 旧增量（页与入队页相同、但共享页已经换代）：丢弃（在下一次整页上传时被覆盖）。
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(AtlasKind::Glyph, 64, 64, vec![0u8; 64 * 2], rows(3, 5));
        assert!(uploads.reconcile_pages([128, 1]).is_empty());
        assert!(!uploads.pending(AtlasKind::Glyph), "旧增量必须丢弃");

        // 4) 与共享页一致的增量：保留（在帧内录制）。
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(AtlasKind::Glyph, 64, 64, vec![0u8; 64 * 2], rows(3, 5));
        assert!(uploads.reconcile_pages([64, 1]).is_empty());
        assert!(uploads.pending(AtlasKind::Glyph), "页对齐的增量要保留");
    }

    /// P1 回归：两帧在飞时各自持有自己的 staging（每窗口 × 每帧槽 × 每图集一块）。
    ///
    /// 两帧上传同一行、先录制后提交：共享 staging 会让第 1 帧提交后读到第 2 帧覆盖的字节。
    #[test]
    fn consecutive_uploads_do_not_share_staging_while_frames_are_in_flight() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut uploads = AtlasUploads::new();
        let pool = command_pool(&gpu);
        let commands = allocate(&gpu, pool, 2);

        // 第 1 帧：整页上传（新页必须整页，DEVIATIONS F-211），内容全零。
        uploads.queue_upload(
            AtlasKind::Image,
            TEST_PAGE,
            gpu.atlas_page(AtlasKind::Image),
            page(),
            full_rects(),
        );
        let rebuild = uploads.reconcile_pages([
            gpu.atlas().page(AtlasKind::Glyph),
            gpu.atlas().page(AtlasKind::Image),
        ]);
        assert_eq!(rebuild, vec![(AtlasKind::Image, TEST_PAGE)]);
        gpu.rebuild_atlas_pages(&rebuild)
            .expect("rebuild image page");
        begin(&gpu, commands[0]);
        let retired = uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record full");
        assert!(
            retired.is_empty(),
            "16² 整页小于常驻上限，不产生 retire 句柄"
        );
        end(&gpu, commands[0]);
        submit_and_wait(&gpu, commands[0]);
        let image = gpu.atlas().image(AtlasKind::Image);
        assert_eq!(read_texel(&gpu, pool, image, 0, 5), [0, 0, 0, 0]);

        // 第 2 帧：第 5 行 = 不透明深蓝（BGRA，打包的 1 行）。先只录制，不提交。
        let mut first_page = page();
        fill_row(&mut first_page, 5, [0x11, 0x22, 0x33, 0xFF]);
        let first_row = first_page[5 * TEST_PAGE as usize * 4..6 * TEST_PAGE as usize * 4].to_vec();
        uploads.queue_upload(
            AtlasKind::Image,
            TEST_PAGE,
            gpu.atlas_page(AtlasKind::Image),
            first_row,
            rows(5, 6),
        );
        begin(&gpu, commands[0]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record frame 2");
        end(&gpu, commands[0]);

        // 第 3 帧：**同一行** = 不透明橙。第 2 帧还没提交：共享 staging 会在这里覆盖字节。
        let mut second_page = page();
        fill_row(&mut second_page, 5, [0x44, 0x55, 0x66, 0xFF]);
        let second_row =
            second_page[5 * TEST_PAGE as usize * 4..6 * TEST_PAGE as usize * 4].to_vec();
        uploads.queue_upload(
            AtlasKind::Image,
            TEST_PAGE,
            gpu.atlas_page(AtlasKind::Image),
            second_row,
            rows(5, 6),
        );
        begin(&gpu, commands[1]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 1, commands[1])
            .expect("record frame 3");
        end(&gpu, commands[1]);

        // 第 2 帧完成：它写进第 5 行的必须是**自己的**深蓝（旧实现会读到橙）。
        submit_and_wait(&gpu, commands[0]);
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 5),
            [0x11, 0x22, 0x33, 0xFF],
            "第 2 帧的 copy 必须读到自己 staging 的字节"
        );

        // 第 3 帧完成：后一帧覆盖前一帧。
        submit_and_wait(&gpu, commands[1]);
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 5),
            [0x44, 0x55, 0x66, 0xFF],
            "后一帧的像素覆盖前一帧"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 6),
            [0, 0, 0, 0],
            "未上传的行保持原样"
        );

        // SAFETY: 队列已空闲（每次提交都 wait idle），命令池没有在飞的缓冲。
        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

    /// R06-3 AOI：只写矩形自己的 `(x, y, w, h)` 区域，页内其它纹素保持原样。
    ///
    /// 旧的「整页宽 × 行区间」语义下，4×1 的矩形也要传 `1 行 × 16 纹素 × 4 B = 64 B`；
    /// AOI 只传 16 B，且左右两侧的纹素不得被写。
    #[test]
    fn an_aoi_rect_upload_only_writes_its_own_texels() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut uploads = AtlasUploads::new();
        let pool = command_pool(&gpu);
        let commands = allocate(&gpu, pool, 1);

        // 第一个整页上传（新页必须整页，DEVIATIONS F-211），内容全零。
        uploads.queue_upload(
            AtlasKind::Image,
            TEST_PAGE,
            gpu.atlas_page(AtlasKind::Image),
            page(),
            full_rects(),
        );
        let rebuild = uploads.reconcile_pages([
            gpu.atlas().page(AtlasKind::Glyph),
            gpu.atlas().page(AtlasKind::Image),
        ]);
        gpu.rebuild_atlas_pages(&rebuild)
            .expect("rebuild image page");
        begin(&gpu, commands[0]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record full");
        end(&gpu, commands[0]);
        submit_and_wait(&gpu, commands[0]);

        // AOI：x 4..8、y 5..6 的 4×1 矩形（BGRA8：4 × 4 = 16 B）。
        let mut rects = DirtyRects::default();
        rects.mark(crate::math::PhysicalRect::new(4, 5, 8, 6));
        assert_eq!(rects.byte_count(TEST_PAGE, 4), 16, "AOI 载荷必须只有 16 B");
        assert_eq!(TEST_PAGE as usize * 4, 64, "旧语义要传 1 整行 = 16 × 4 B");
        let mut payload = Vec::with_capacity(16);
        for _ in 0..4 {
            payload.extend_from_slice(&[0x11, 0x22, 0x33, 0xFF]);
        }
        uploads.queue_upload(
            AtlasKind::Image,
            TEST_PAGE,
            gpu.atlas_page(AtlasKind::Image),
            payload,
            rects,
        );
        begin(&gpu, commands[0]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record aoi");
        end(&gpu, commands[0]);
        submit_and_wait(&gpu, commands[0]);

        let image = gpu.atlas().image(AtlasKind::Image);
        assert_eq!(
            read_texel(&gpu, pool, image, 3, 5),
            [0, 0, 0, 0],
            "AOI 左侧一列不得被写"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 4, 5),
            [0x11, 0x22, 0x33, 0xFF],
            "AOI 首列"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 7, 5),
            [0x11, 0x22, 0x33, 0xFF],
            "AOI 末列"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 8, 5),
            [0, 0, 0, 0],
            "AOI 右侧一列不得被写"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 4, 4),
            [0, 0, 0, 0],
            "AOI 上一行不得被写"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 4, 6),
            [0, 0, 0, 0],
            "AOI 下一行不得被写"
        );

        // SAFETY: 队列已空闲（每次提交都 wait idle），命令池没有在飞的缓冲。
        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

    /// miss 帧（录制失败 / 未提交）的 retire 句柄由 teardown 的 `drain_all_retired` 兜底释放。
    #[test]
    fn teardown_drains_retired_resources_of_a_frame_that_never_submitted() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut sync = FrameSync::new(&gpu).expect("frame sync");
        // 两个真实的设备对象（buffer + memory）：retire 之后必须由 teardown 真正销毁。
        // SAFETY: 句柄归本测试独占；销毁走 `FrameSync` 的 retire / drain 路径。
        let (buffer, memory) = unsafe {
            let buffer = gpu
                .raw()
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(16)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .expect("buffer");
            let requirements = gpu.raw().get_buffer_memory_requirements(buffer);
            let memory_type = gpu
                .memory_type(requirements.memory_type_bits, MemoryNeed::HOST_WRITABLE)
                .expect("host-visible memory");
            let memory = gpu
                .raw()
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type),
                    None,
                )
                .expect("memory");
            gpu.raw()
                .bind_buffer_memory(buffer, memory, 0)
                .expect("bind");
            (buffer, memory)
        };
        let buffer_handle = gpu.allocate(GpuObject::Buffer(buffer));
        let memory_handle = gpu.allocate(GpuObject::Memory(memory));
        sync.retire(7, buffer_handle);
        sync.retire(7, memory_handle);
        assert_eq!(sync.pending_retired(), 2);
        assert_eq!(gpu.live_resources(), 2);

        // 该帧从未提交：完成前沿到不了 7，正常 drain 不释放。
        assert_eq!(sync.drain_retire(6), 0);
        assert_eq!(gpu.live_resources(), 2);

        // teardown：一次性释放（buffer 先于 memory，顺序由 retire 队列保持）。
        assert_eq!(sync.drain_all_retired(), 2);
        assert_eq!(sync.pending_retired(), 0);
        assert_eq!(gpu.live_resources(), 0);
    }
}

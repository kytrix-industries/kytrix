//! 共享图集纹理（[`SharedAtlas`]）+ 每窗口上传器（[`AtlasUploads`]）
//! （P1-2.9 / D-9-c）。
//!
//! 字形图集是 `R8_UNORM`（线性覆盖度），图片图集是 `B8G8R8A8_SRGB`。两张纹理都是 **texture
//! array**（字形固定 1 层；图片的层数 = App 当前打开的图集页数，`AtlasArraySpec`，随页数增长；
//! 层号 = 图集页索引）且是**进程级共享资源**，由 `device::SharedGpu` 持有；每窗口只有自己的
//! **上传路径**：
//!
//! - [`SharedAtlas`]：两张 texture array + 一个 sampler + 当前页尺寸 / 层数。懒分配：初始化只建
//!   1×1×1 占位页（描述符集合里始终有有效视图，4 B 而不是 16 MiB），第一次整页上传 /
//!   页档换代 / **层数增长**时由 `SharedGpu::rebuild_atlas_pages` 重建。新页在写锁内先转到
//!   `SHADER_READ_ONLY_OPTIMAL`，因此所有窗口的增量上传都走同一条
//!   `SHADER_READ_ONLY → TRANSFER_DST → SHADER_READ_ONLY` 屏障序列 —— 不需要「哪张页初始化过」
//!   的可变状态（那是跨窗口竞态）。
//! - [`AtlasUploads`]（每窗口）：待上传层（每 kind 每层一份）+ 每 in-flight 槽 × 每图集的
//!   staging。staging 必须是每窗口的：同一份共享纹理会被多个窗口的 GPU 线程并发上传，各窗口
//!   的 staging 不能互相覆盖；每个槽的 staging 又只在本窗口该槽的 fence 完成前复用（与
//!   `UploadArena` 同一套 in-flight 语义）。同一帧的多层上传**聚合进同一块 staging**：每层
//!   每个脏矩形各占自己的偏移（共用 offset 0 会让 GPU 只读到最后一层），逐矩形把
//!   `vkCmdCopyBufferToImage` 的 `base_array_layer` 写成层号。
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
    paint::atlas::{AtlasKind, MAX_IMAGE_PAGES},
    render::{
        AtlasArraySpec, AtlasPageRequest,
        atlas::{DirtyRects, MAX_RESIDENT_STAGING_BYTES, RectChunk, texel_bytes},
        frame::MAX_FRAMES_IN_FLIGHT,
        linux::{
            device::{Device, GpuObject, SharedGpu, init_error},
            memory::MemoryNeed,
        },
        merge_page_request,
        retire::GpuResourceHandle,
    },
};

/// 初始化时的占位页（1×1×1）：让描述符集合里的图集视图在第一次上传之前也有效。
const PLACEHOLDER_PAGE: u32 = 1;

/// 一张共享图集页（texture array + image + memory + view + 页尺寸）。
struct SharedPage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    page: u32,
    /// texture array 层数（字形 1；图片 = App 当前打开的图集页数）。层号 = 图集页索引。
    layers: u32,
}

impl SharedPage {
    fn new(
        device: &Device,
        kind: AtlasKind,
        spec: crate::render::AtlasArraySpec,
    ) -> Result<Self, EngineError> {
        let format = texture_format(kind);
        let page = spec.page;
        let layers = spec.layers;
        debug_assert!(layers > 0, "texture array must have at least one layer");
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
                    .array_layers(layers)
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
                    // D-9-c：视图覆盖整条 texture array（shader 侧声明 sampler2DArray）。
                    .view_type(vk::ImageViewType::TYPE_2D_ARRAY)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(layers),
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
            layers,
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
    /// 建两张 1×1×1 占位页 + sampler；真实页由 `SharedGpu::rebuild_atlas_pages` 生效。
    pub(crate) fn new(device: &Device) -> Result<Self, EngineError> {
        let placeholder = crate::render::AtlasArraySpec {
            page: PLACEHOLDER_PAGE,
            layers: 1,
        };
        let glyph = SharedPage::new(device, AtlasKind::Glyph, placeholder)?;
        let image = match SharedPage::new(device, AtlasKind::Image, placeholder) {
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

    /// 该图集 texture array 的层数（录制屏障必须覆盖整个数组；字形 1、图片 = 当前页数）。
    pub(crate) fn layers(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).layers
    }

    /// 该图集当前的 array 规格（页边长 + 层数）—— 上传器对齐 / 重建判据。
    pub(crate) fn spec(&self, kind: AtlasKind) -> AtlasArraySpec {
        let page = self.page_ref(kind);
        AtlasArraySpec {
            page: page.page,
            layers: page.layers,
        }
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

    /// 换 array：按新规格（页边长 + 层数）建新 image / memory / view 并替换（调用方已等共享
    /// 队列 idle，旧句柄立即销毁）。层数增长时旧层内容由 App 在该帧对每个已打开页整页重传补齐。
    pub(crate) fn recreate(
        &mut self,
        device: &Device,
        kind: AtlasKind,
        spec: crate::render::AtlasArraySpec,
    ) -> Result<(), EngineError> {
        let next = SharedPage::new(device, kind, spec)?;
        let old = std::mem::replace(self.page_mut(kind), next);
        old.destroy(&self.device);
        Ok(())
    }

    /// 录制「新建页从 `UNDEFINED` 转到 `SHADER_READ_ONLY_OPTIMAL`」的屏障。
    ///
    /// 只对 `kinds` 里的页发屏障（占位页初始化 / 每次重建的新页）；每条屏障覆盖整条 texture
    /// array（数组各层的布局都是 `UNDEFINED`）。
    pub(crate) fn record_layout_transitions(
        &self,
        device: &ash::Device,
        command: vk::CommandBuffer,
        kinds: &[AtlasKind],
    ) {
        let barriers: Vec<vk::ImageMemoryBarrier> = kinds
            .iter()
            .map(|kind| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .image(self.image(*kind))
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(self.layers(*kind)),
                    )
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

/// 待上传的一层内容（打包 `pixels` + 层号 + array 规格 + 脏矩形）。
struct PendingUpload {
    /// texture array 层号（字形恒 0；图片 = 图集页索引）。
    layer: u16,
    /// 入队时 App 要求的 array 规格（页边长 + 层数）。层数可能大于当前数组层数（新页刚打开）
    /// ⇒ 录制前必须先扩层。
    spec: crate::render::AtlasArraySpec,
    /// 入队时**共享图集**的页尺寸。
    ///
    /// 用来区分「新页」（App 换代后要重建共享纹理，`spec.page != queued_page`）与「旧页」
    /// （入队后别的窗口已经换代，`spec.page == queued_page` 且与当前共享页不同：上传作废）。
    queued_page: u32,
    /// `rects` 顺序逐矩形紧密打包的字节（整页上传时就是整页）。
    pixels: Vec<u8>,
    rects: DirtyRects,
}

impl PendingUpload {
    /// 是否覆盖整页（首次上传 / 页换代 / 扩层之后的唯一合法载荷）。
    fn covers_full_page(&self) -> bool {
        self.rects.covers_page(self.spec.page)
    }
}

/// 每窗口的图集上传路径：待上传层（每 kind 每层一份）+ 每 in-flight 槽 × 每图集的 staging。
pub(crate) struct AtlasUploads {
    /// `[kind][layer]`：字形只有 `[0]`，图片 `0..MAX_IMAGE_PAGES`（页数上限；数组当前层数
    /// 更小，入队断言用 `spec.layers`）。
    pending: [[Option<PendingUpload>; MAX_IMAGE_PAGES as usize]; 2],
    staging: [StagingSet; 2],
}

impl AtlasUploads {
    pub(crate) fn new() -> Self {
        Self {
            pending: std::array::from_fn(|_| std::array::from_fn(|_| None)),
            staging: [
                StagingSet::new(AtlasKind::Glyph),
                StagingSet::new(AtlasKind::Image),
            ],
        }
    }

    /// 该图集是否有**任意层**的上传已入队、还没被命令缓冲消费（离屏目标的空闲判据）。
    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        self.pending[kind.index() as usize]
            .iter()
            .any(Option::is_some)
    }

    /// 入队一层的脏矩形（`pixels` 是 `rects` 顺序**逐矩形紧密打包**的字节，R06-3）。
    ///
    /// `spec` 是 App 要求的 array 规格（页边长 + 层数）：层号硬断言按**运行时层数**校验
    /// （图片当前打开的页数可以小于上限），不是 `MAX_IMAGE_PAGES` 常量。
    /// `pixels` 按**所有权**移动进待上传槽，避免槽 → 纹理之间再复制一次（2048² 图片页是
    /// 16 MiB）。长度必须等于 [`DirtyRects::byte_count`]（release 也硬断言：越界读会读穿堆）。
    pub(crate) fn queue_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        spec: crate::render::AtlasArraySpec,
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
    /// 无论整页还是增量都直接丢弃 —— 重建回旧页会让所有窗口对尺寸错误的纹理采样。
    ///
    /// 调用方在 `SharedGpu` 的图集读锁内调用；返回非空时先释放读锁，
    /// `SharedGpu::rebuild_atlas_pages` 在写锁内落地新 array，然后重试。
    pub(crate) fn reconcile_pages(
        &mut self,
        current: [crate::render::AtlasArraySpec; 2],
    ) -> Vec<AtlasPageRequest> {
        let mut rebuild: Vec<AtlasPageRequest> = Vec::with_capacity(2);
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let current = current[index];
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
        rebuild
    }

    /// 在本帧命令缓冲里录制全部待上传层（每层 = staging 写入 + 逐区间
    /// `cmd_copy_buffer_to_image`）。
    ///
    /// 同一 kind 的多层在一次调用里**聚合**：共用一块 staging，每层每个脏矩形的拷贝各占自己的
    /// 偏移（共用 offset 0 会让 GPU 只读到最后一层），逐矩形把
    /// `cmd_copy_buffer_to_image` 的 `base_array_layer` 写成层号（D-9-c 多页 texture array）。
    /// 屏障按 kind 各一组、覆盖整条数组。
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
            let pendings: Vec<PendingUpload> = self.pending[kind_index]
                .iter_mut()
                .filter_map(Option::take)
                .collect();
            if pendings.is_empty() {
                continue;
            }
            let page = atlas.page(kind);
            debug_assert!(
                pendings.iter().all(|pending| pending.spec.page == page),
                "page changes must be applied before recording"
            );
            debug_assert!(
                pendings
                    .iter()
                    .all(|pending| u32::from(pending.layer) < atlas.layers(kind)),
                "pending layers must be inside the shared texture array"
            );
            if pendings.iter().any(|pending| pending.spec.page != page) {
                // 防御：页在录制期间被改（理论上被图集读锁排除）。丢掉而不是画错内容。
                continue;
            }
            self.staging[kind_index].record(
                shared,
                atlas,
                slot,
                command,
                &pendings,
                &mut retired,
            )?;
        }
        Ok(retired)
    }
}

/// 一个待拷贝矩形在 staging 里的落点（staging 偏移 + 目标数组层号 + 源待上传层下标）。
struct StagingRect {
    chunk: RectChunk,
    /// staging 里的起始字节偏移。
    ///
    /// 同帧多层**共用**一块 staging，每个矩形必须有自己的偏移：共用 offset 0 会让 GPU 只读到
    /// 最后写入的那一层。
    offset: u64,
    /// 目标 texture array 层号（D-9-c：层号 = 图集页索引）。
    layer: u16,
    /// 源 [`PendingUpload`] 在本次聚合里的下标。
    pending: usize,
}

/// 一个图集在每 in-flight 槽上的常驻 staging。
struct StagingSet {
    kind: AtlasKind,
    slots: [Option<StagingRing>; MAX_FRAMES_IN_FLIGHT],
}

impl StagingSet {
    const fn new(kind: AtlasKind) -> Self {
        Self {
            kind,
            slots: [None, None, None],
        }
    }

    /// 录制本帧该图集**全部待上传层**的上传：staging 写入 + 屏障 + 逐矩形
    /// `cmd_copy_buffer_to_image` + 屏障。
    ///
    /// AOI（R06-3）：每个脏矩形单独一次 `vkCmdCopyBufferToImage`，`image_offset` 与
    /// `image_extent` 都取矩形自己的 `(x, y, w, h)`（缺 `image_offset` 会从第 0 行覆写整页；
    /// 用整页宽做 `image_extent` 就是旧的放大根因）。
    ///
    /// D-9-c：同一 kind 的多层聚合成**一次**调用 —— 共用本槽一块 staging（每层每个矩形各占
    /// 自己的偏移），逐矩形把目标纹理数组的层号写进 `base_array_layer`；屏障每 kind 只有一组
    /// （覆盖整条数组），因此同一帧的多层不会各自转一次布局。
    fn record(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        slot: usize,
        command: vk::CommandBuffer,
        pendings: &[PendingUpload],
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        let kind = self.kind;
        let image = atlas.image(kind);
        // D-9-c：屏障覆盖整条 texture array（每 kind 一次 to_dst / to_sampled）。
        let layers = atlas.layers(kind);
        let texel = texel_bytes(kind);
        // staging 布局：每块起点按 4 字节对齐（Vulkan 要求 bufferOffset 是纹素块大小的倍数，
        // 4 对齐对 R8/BGRA8 都成立），块内行紧密排列（`bufferRowLength = 0`）。
        let mut layout: Vec<StagingRect> = Vec::new();
        let mut length = 0usize;
        for (pending_index, pending) in pendings.iter().enumerate() {
            for chunk in pending.rects.chunks(pending.spec.page, texel) {
                let offset = length.next_multiple_of(4);
                length = offset + (chunk.width * chunk.height) as usize * texel as usize;
                layout.push(StagingRect {
                    chunk,
                    offset: offset as u64,
                    layer: pending.layer,
                    pending: pending_index,
                });
            }
        }
        if layout.is_empty() {
            // 空集合 = 零上传：不分配 staging、不录 copy。
            return Ok(());
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
        // SAFETY: 映射在创建时常驻，`offset + 块字节数 ≤ length ≤ 容量`；每个
        // `pending.pixels` 的长度已由 `queue_upload` 的 `byte_count` 断言保证，
        // `chunk.offset + 块字节数` 在界内。
        unsafe {
            for rect in &layout {
                let bytes = (rect.chunk.width * rect.chunk.height) as usize * texel as usize;
                std::ptr::copy_nonoverlapping(
                    pendings[rect.pending]
                        .pixels
                        .as_ptr()
                        .add(rect.chunk.offset),
                    staging_ptr.add(rect.offset as usize),
                    bytes,
                );
            }
        }

        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            // D-9-c：屏障覆盖整条 texture array（每 kind 一次 to_dst / to_sampled）。
            .layer_count(layers);
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
            for rect in &layout {
                shared.raw().cmd_copy_buffer_to_image(
                    command,
                    staging_buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy::default()
                        // 每块在 staging 里各有偏移（块间 4 字节对齐）。
                        .buffer_offset(rect.offset)
                        // 0 = 行按 `image_extent.width` 紧密排列（块内只有 width × texel 字节）。
                        .buffer_row_length(0)
                        .buffer_image_height(0)
                        // AOI：目标从矩形自己的 (x, y) 开始；z 恒 0，层号由
                        // `base_array_layer` 指定。
                        .image_offset(vk::Offset3D {
                            x: rect.chunk.x as i32,
                            y: rect.chunk.y as i32,
                            z: 0,
                        })
                        .image_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                // D-9-c：目标数组层号 = 图集页索引。
                                .base_array_layer(u32::from(rect.layer))
                                .layer_count(1),
                        )
                        .image_extent(vk::Extent3D {
                            width: rect.chunk.width,
                            height: rect.chunk.height,
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

    /// 读回图集 texture array 某一层某个纹素的原始存储字节（BGRA，不做通道交换）。
    fn read_texel(
        gpu: &SharedGpu,
        pool: vk::CommandPool,
        image: vk::Image,
        layer: u16,
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
            // D-9-c：只转场 / 读回指定的数组层（base_array_layer = layer）。
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .base_array_layer(u32::from(layer))
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
                            .base_array_layer(u32::from(layer))
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

    /// 图集 array 规格（测试页 + 层数）。
    fn spec(page: u32, layers: u32) -> crate::render::AtlasArraySpec {
        crate::render::AtlasArraySpec { page, layers }
    }

    /// 只含一个 kind 的重建请求（断言用）。
    fn request(kind: AtlasKind, page: u32, layers: u32) -> crate::render::AtlasPageRequest {
        crate::render::AtlasPageRequest {
            kind,
            spec: spec(page, layers),
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
            0,
            spec(64, 1),
            1,
            vec![0u8; (64 * 64) as usize],
            full_rects(),
        );
        assert_eq!(
            uploads.reconcile_pages([spec(1, 1), spec(1, 1)]),
            vec![request(AtlasKind::Glyph, 64, 1)]
        );
        assert!(uploads.pending(AtlasKind::Glyph), "新页要留到重建之后上传");

        // 2) 旧整页（入队时共享页就是它，之后别的窗口换代）：丢弃；不把共享纹理重建回旧页。
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            0,
            spec(64, 1),
            64,
            vec![0u8; (64 * 64) as usize],
            full_rects(),
        );
        assert!(
            uploads
                .reconcile_pages([spec(128, 1), spec(1, 1)])
                .is_empty()
        );
        assert!(!uploads.pending(AtlasKind::Glyph), "旧整页必须丢弃");

        // 3) 旧增量（页与入队页相同、但共享页已经换代）：丢弃（在下一次整页上传时被覆盖）。
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            0,
            spec(64, 1),
            64,
            vec![0u8; 64 * 2],
            rows(3, 5),
        );
        assert!(
            uploads
                .reconcile_pages([spec(128, 1), spec(1, 1)])
                .is_empty()
        );
        assert!(!uploads.pending(AtlasKind::Glyph), "旧增量必须丢弃");

        // 4) 与共享页一致的增量：保留（在帧内录制）。
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            0,
            spec(64, 1),
            64,
            vec![0u8; 64 * 2],
            rows(3, 5),
        );
        assert!(
            uploads
                .reconcile_pages([spec(64, 1), spec(1, 1)])
                .is_empty()
        );
        assert!(uploads.pending(AtlasKind::Glyph), "页对齐的增量要保留");
    }

    /// D-9-c：同一 extent 的多层整页上传只产生**一次**重建请求（4 层图片图集共用数组）。
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
            uploads.reconcile_pages([spec(1, 1), spec(1, 1)]),
            vec![request(AtlasKind::Image, TEST_PAGE, 4)],
            "按 extent 去重：数组只有一次重建，且层数取最大值"
        );
        assert!(
            (0..4).all(|layer| uploads.pending[1][layer].is_some()),
            "四层都要保留到重建之后上传"
        );
    }

    /// 越界层号必须被**动态**层数硬断言拦住：数组只有 2 层时，layer 2 越界
    /// （变异：断言改回常量上界 ⇒ layer 2 通过 ⇒ 本测试不 panic 而失败）。
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
            0,
            spec(TEST_PAGE, 1),
            gpu.atlas_page(AtlasKind::Image),
            page(),
            full_rects(),
        );
        let rebuild = uploads.reconcile_pages([
            gpu.atlas().spec(AtlasKind::Glyph),
            gpu.atlas().spec(AtlasKind::Image),
        ]);
        assert_eq!(rebuild, vec![request(AtlasKind::Image, TEST_PAGE, 1)]);
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
        assert_eq!(read_texel(&gpu, pool, image, 0, 0, 5), [0, 0, 0, 0]);

        // 第 2 帧：第 5 行 = 不透明深蓝（BGRA，打包的 1 行）。先只录制，不提交。
        let mut first_page = page();
        fill_row(&mut first_page, 5, [0x11, 0x22, 0x33, 0xFF]);
        let first_row = first_page[5 * TEST_PAGE as usize * 4..6 * TEST_PAGE as usize * 4].to_vec();
        uploads.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
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
            0,
            spec(TEST_PAGE, 1),
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
            read_texel(&gpu, pool, image, 0, 0, 5),
            [0x11, 0x22, 0x33, 0xFF],
            "第 2 帧的 copy 必须读到自己 staging 的字节"
        );

        // 第 3 帧完成：后一帧覆盖前一帧。
        submit_and_wait(&gpu, commands[1]);
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 0, 5),
            [0x44, 0x55, 0x66, 0xFF],
            "后一帧的像素覆盖前一帧"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 0, 6),
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
            0,
            spec(TEST_PAGE, 1),
            gpu.atlas_page(AtlasKind::Image),
            page(),
            full_rects(),
        );
        let rebuild = uploads.reconcile_pages([
            gpu.atlas().spec(AtlasKind::Glyph),
            gpu.atlas().spec(AtlasKind::Image),
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
            0,
            spec(TEST_PAGE, 1),
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
            read_texel(&gpu, pool, image, 0, 3, 5),
            [0, 0, 0, 0],
            "AOI 左侧一列不得被写"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 4, 5),
            [0x11, 0x22, 0x33, 0xFF],
            "AOI 首列"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 7, 5),
            [0x11, 0x22, 0x33, 0xFF],
            "AOI 末列"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 8, 5),
            [0, 0, 0, 0],
            "AOI 右侧一列不得被写"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 4, 4),
            [0, 0, 0, 0],
            "AOI 上一行不得被写"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 4, 6),
            [0, 0, 0, 0],
            "AOI 下一行不得被写"
        );

        // SAFETY: 队列已空闲（每次提交都 wait idle），命令池没有在飞的缓冲。
        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

    /// D-9-c：图片图集是多层 texture array（层数 = 打开的页数），`vkCmdCopyBufferToImage` 的目标层号走
    /// `base_array_layer`。
    ///
    /// 每层写不同的颜色并在**同一帧**里聚合录制、随后逐层读回：证明 ① 纹理数组真的有 4 层；
    /// ② 层号进了目标子资源；③ 同帧多层共用一块 staging 时各有偏移、不会互相覆盖。
    #[test]
    fn image_atlas_layers_upload_to_their_own_array_slices() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut uploads = AtlasUploads::new();
        let pool = command_pool(&gpu);
        let commands = allocate(&gpu, pool, 1);
        const COLORS: [[u8; 4]; 4] = [
            [0x11, 0x22, 0x33, 0xFF],
            [0x44, 0x55, 0x66, 0xFF],
            [0x77, 0x88, 0x99, 0xFF],
            [0xAA, 0xBB, 0xCC, 0xFF],
        ];
        for (layer, color) in COLORS.iter().enumerate() {
            let mut pixels = page();
            for texel in pixels.as_chunks_mut::<4>().0 {
                *texel = *color;
            }
            uploads.queue_upload(
                AtlasKind::Image,
                layer as u16,
                spec(TEST_PAGE, 4),
                gpu.atlas_page(AtlasKind::Image),
                pixels,
                full_rects(),
            );
        }
        let rebuild = uploads.reconcile_pages([
            gpu.atlas().spec(AtlasKind::Glyph),
            gpu.atlas().spec(AtlasKind::Image),
        ]);
        assert_eq!(
            rebuild,
            vec![request(AtlasKind::Image, TEST_PAGE, 4)],
            "四层同 extent 只请求一次重建（层数取最大值）"
        );
        gpu.rebuild_atlas_pages(&rebuild)
            .expect("rebuild image page");
        assert_eq!(
            gpu.atlas().layers(AtlasKind::Image),
            4,
            "请求 4 层 ⇒ texture array 必须建成 4 层"
        );
        begin(&gpu, commands[0]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record four layers");
        end(&gpu, commands[0]);
        submit_and_wait(&gpu, commands[0]);

        let image = gpu.atlas().image(AtlasKind::Image);
        for (layer, color) in COLORS.iter().enumerate() {
            let layer = layer as u16;
            assert_eq!(
                read_texel(&gpu, pool, image, layer, 0, 0),
                *color,
                "第 {layer} 层的首纹素必须来自该层的上传"
            );
            assert_eq!(
                read_texel(&gpu, pool, image, layer, TEST_PAGE - 1, TEST_PAGE - 1),
                *color,
                "第 {layer} 层的末纹素必须来自该层的上传"
            );
        }

        // SAFETY: 队列已空闲（每次提交都 wait idle），命令池没有在飞的缓冲。
        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

    /// 页数 1 → 2（P2-B3 核心语义）：数组从 1 层重建为 **2 层**，且第 0 层内容仍在。
    ///
    /// 扩层帧 App 对每个已打开页整页重传（`App::upload_image_page` 的 `array_missing` 分支）。
    /// 变异：`reconcile_pages` 不识别 `spec.layers > current.layers` ⇒ 不重建（数组仍 1 层、
    /// 层数断言失败，写 layer 1 还会越界）。
    #[test]
    fn growing_the_image_array_keeps_the_existing_layer_content() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut uploads = AtlasUploads::new();
        let pool = command_pool(&gpu);
        let commands = allocate(&gpu, pool, 1);
        const LAYER_ZERO: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];
        const LAYER_ONE: [u8; 4] = [0x44, 0x55, 0x66, 0xFF];
        let fill = |colour: [u8; 4]| {
            let mut pixels = page();
            for texel in pixels.as_chunks_mut::<4>().0 {
                *texel = colour;
            }
            pixels
        };

        // 第 1 页：1 层数组，layer 0 = 颜色 A。
        uploads.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            gpu.atlas_page(AtlasKind::Image),
            fill(LAYER_ZERO),
            full_rects(),
        );
        let rebuild = uploads.reconcile_pages([
            gpu.atlas().spec(AtlasKind::Glyph),
            gpu.atlas().spec(AtlasKind::Image),
        ]);
        assert_eq!(rebuild, vec![request(AtlasKind::Image, TEST_PAGE, 1)]);
        gpu.rebuild_atlas_pages(&rebuild)
            .expect("rebuild image page");
        assert_eq!(gpu.atlas().layers(AtlasKind::Image), 1, "1 页 ⇒ 1 层");
        begin(&gpu, commands[0]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record layer 0");
        end(&gpu, commands[0]);
        submit_and_wait(&gpu, commands[0]);
        let image = gpu.atlas().image(AtlasKind::Image);
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 0, 0),
            LAYER_ZERO,
            "1 层数组的第 0 层内容"
        );

        // 开第 2 页：扩层到 2 层，同一帧把两个已打开页整页重传。
        for (layer, colour) in [(0u16, LAYER_ZERO), (1, LAYER_ONE)] {
            uploads.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 2),
                gpu.atlas_page(AtlasKind::Image),
                fill(colour),
                full_rects(),
            );
        }
        let rebuild = uploads.reconcile_pages([
            gpu.atlas().spec(AtlasKind::Glyph),
            gpu.atlas().spec(AtlasKind::Image),
        ]);
        assert_eq!(
            rebuild,
            vec![request(AtlasKind::Image, TEST_PAGE, 2)],
            "层数增长必须请求重建为 2 层"
        );
        gpu.rebuild_atlas_pages(&rebuild).expect("grow image array");
        assert_eq!(
            gpu.atlas().layers(AtlasKind::Image),
            2,
            "页数 1 → 2 ⇒ texture array 必须重建为 2 层"
        );
        begin(&gpu, commands[0]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record both layers");
        end(&gpu, commands[0]);
        submit_and_wait(&gpu, commands[0]);
        let image = gpu.atlas().image(AtlasKind::Image);
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 0, 0),
            LAYER_ZERO,
            "重建后第 0 层内容必须仍在（增长不能丢已上传的图）"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 1, 0, 0),
            LAYER_ONE,
            "第 1 层（新页）内容必须正确"
        );

        // SAFETY: 队列已空闲（每次提交都 wait idle），命令池没有在飞的缓冲。
        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

    /// D-9-c：之后只上传第 1 层时，第 0 层的内容必须原样保留（层号没有写错子资源）。
    #[test]
    fn uploading_one_image_layer_preserves_the_other_layers() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut uploads = AtlasUploads::new();
        let pool = command_pool(&gpu);
        let commands = allocate(&gpu, pool, 1);
        const LAYER_ZERO: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];
        const LAYER_ONE: [u8; 4] = [0x44, 0x55, 0x66, 0xFF];

        // 新页：第 0 层整页写第一种颜色。
        let mut first = page();
        for texel in first.as_chunks_mut::<4>().0 {
            *texel = LAYER_ZERO;
        }
        uploads.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 2),
            gpu.atlas_page(AtlasKind::Image),
            first,
            full_rects(),
        );
        let rebuild = uploads.reconcile_pages([
            gpu.atlas().spec(AtlasKind::Glyph),
            gpu.atlas().spec(AtlasKind::Image),
        ]);
        gpu.rebuild_atlas_pages(&rebuild)
            .expect("rebuild image page");
        begin(&gpu, commands[0]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record layer 0");
        end(&gpu, commands[0]);
        submit_and_wait(&gpu, commands[0]);
        let image = gpu.atlas().image(AtlasKind::Image);
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 0, 0),
            LAYER_ZERO,
            "第 0 层的整页上传"
        );

        // 第二帧：只上传第 1 层整页。
        let mut second = page();
        for texel in second.as_chunks_mut::<4>().0 {
            *texel = LAYER_ONE;
        }
        uploads.queue_upload(
            AtlasKind::Image,
            1,
            spec(TEST_PAGE, 2),
            gpu.atlas_page(AtlasKind::Image),
            second,
            full_rects(),
        );
        begin(&gpu, commands[0]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record layer 1");
        end(&gpu, commands[0]);
        submit_and_wait(&gpu, commands[0]);
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 0, 0),
            LAYER_ZERO,
            "上传第 1 层不得改动第 0 层"
        );
        assert_eq!(
            read_texel(&gpu, pool, image, 1, 0, 0),
            LAYER_ONE,
            "第 1 层内容必须正确"
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

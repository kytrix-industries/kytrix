//! 图集纹理：字形 `R8Unorm` + 图片 `BGRA8Unorm_sRGB` 的 **texture array**（D-9-c），脏矩形（AOI）
//! 上传。
//!
//! 字形图集恒 **1 层**；图片图集的层数 = App 当前打开的图集页数（`ImageAtlasPager::page_count`，
//! [`crate::render::AtlasArraySpec`]），**随页数增长**（只增不减）。**层号 = 图集页索引**，shader
//! 用 `params1.y` 作 texture array 的层号采样（三端同一契约）。像素来源由上层持有（P05 §10.6 的
//! 定案：渲染层不保 CPU 副本）；本模块只做「staging 写 → blit 到数组层」与「规格变化时重建数组
//! + 整页重传」。懒创建：没有图片时图片图集是 1×1×1 的占位数组（4 B，不是 16 MiB）。
//!
//! **staging 是每 in-flight 槽一块常驻的 `MTLBuffer`**（DEVIATIONS F-212）：槽位复用由
//! `Queue::begin` 的三槽 permit 保证「上一帧已完成」，因此本帧可以直接覆盖写；容量不够时
//! 重建并把旧缓冲按提交序号 retire。上传是低频路径、每帧至多一次，常驻一块按需扩容的缓冲
//! 比每帧分配更省（也避免分配抖动）。超过 1 MiB 的上传（整页 / 同帧多层）改用一次性缓冲并按
//! 提交序号 retire（F-216），常驻容量因此不随页尺寸放大。
//!
//! 同一帧的多层上传**聚合进一块 staging、各自偏移**：每层每个脏矩形有自己的 `(offset, row_pitch)`，
//! blit 的 `destinationSlice` 写层号。绝不能把多层都放在 offset 0 —— GPU 只会读到最后一层
//! （与 D3D12 V-64 同类）。
//!
//! retire 的键是 `u64` 提交序号（`FrameSlot::signal_value`），**不是** `FrameIndex`（R06-1）：
//! 帧序号会饱和，饱和后按它记账会提前释放只在飞帧仍在采样的纹理 / 缓冲。

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLDevice, MTLOrigin, MTLPixelFormat, MTLResourceOptions,
    MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};

use crate::{
    error::EngineError,
    paint::atlas::{AtlasKind, MAX_IMAGE_PAGES},
    render::{
        atlas::{AtlasRebuilt, DirtyRects, MAX_RESIDENT_STAGING_BYTES, RectChunk, texel_bytes},
        frame::MAX_FRAMES_IN_FLIGHT,
        macos::{
            device::{Device, GpuObject, init_error},
            queue::Queue,
        },
        retire::GpuResourceHandle,
    },
};

/// 初始化时的占位页（1×1×1）：让参数缓冲里的图集纹理在第一次上传之前也有效。
const PLACEHOLDER_PAGE: u32 = 1;

/// staging 的每块起点 / 行 pitch 的对齐：Metal 的 buffer→texture 拷要求行 pitch 是像素尺寸的
/// 倍数，4 对齐对 `R8Unorm`（1 B）与 `BGRA8Unorm_sRGB`（4 B）都成立。
const METAL_STAGING_ALIGNMENT: usize = 4;

/// 一个 AOI 矩形在本帧 staging 里的落点。
#[derive(Clone, Copy)]
struct MetalRect {
    /// 该矩形来自哪一个待上传层（`pendings` 的下标）。同一帧的多层共用一块 staging，
    /// 每层写入各自偏移，不能共用 offset 0（否则 GPU 只读到最后一层）。
    pending: usize,
    /// 目标 texture array 层号（`copyFromBuffer` 的 `destinationSlice`）。
    layer: u16,
    chunk: RectChunk,
    /// staging 里的起始字节偏移（[`METAL_STAGING_ALIGNMENT`] 对齐）。
    offset: usize,
    /// staging 里该矩形的行 pitch（字节，[`METAL_STAGING_ALIGNMENT`] 对齐）。
    row_pitch: usize,
}

/// 一张图集页（texture array）。
struct AtlasTexture {
    kind: AtlasKind,
    /// texture array 的层数（字形 1、图片 = App 当前打开的图集页数）：层号 = 图集页索引。
    layers: u32,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    /// 每 in-flight 槽一块常驻 staging（DEVIATIONS F-212）：槽位复用由 `Queue::begin` 的
    /// 三槽 semaphore 保证「上一帧已完成」，因此可以覆盖写，无需按帧 retire。
    staging: [Option<StagingBuffer>; MAX_FRAMES_IN_FLIGHT],
    /// 占位页没有句柄（它从未进入任何帧）；真实页重建时旧句柄按提交序号 retire。
    handle: Option<GpuResourceHandle>,
    page: u32,
    /// `[layer]`：每层一个待上传槽（一帧内可能同帧上传多个层）。长度 ≥ 数组当前层数
    /// （入队时按 `AtlasArraySpec::layers` 扩到需要的长度）。
    pending: Vec<Option<PendingUpload>>,
}

/// 每 in-flight 槽一块常驻 staging（`StorageModeShared`，CPU 直接写）。
struct StagingBuffer {
    buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
    handle: GpuResourceHandle,
    capacity: usize,
}

/// 待上传的一层（打包 `pixels` + 层号 + array 规格 + 脏矩形）。
#[derive(Debug)]
struct PendingUpload {
    /// texture array 层号（字形恒 0；图片 = 图集页索引）。
    layer: u16,
    /// 入队时 App 要求的 array 规格（页边长 + 层数）。层数可能大于当前数组层数（新页刚打开）
    /// ⇒ `prepare` 必须先扩层再 blit。
    spec: crate::render::AtlasArraySpec,
    /// `rects` 顺序逐矩形紧密打包的字节（整页上传时就是整页）。
    pixels: Vec<u8>,
    rects: DirtyRects,
}

/// 两张 texture array（字形 / 图片）。
pub(crate) struct AtlasTextures {
    glyph: AtlasTexture,
    image: AtlasTexture,
    /// `[kind][layer]`：`prepare` 重建过纹理之后，本帧该层必须整页上传（新数组内容未定义）。
    /// 下标上界 = `MAX_IMAGE_PAGES`（页数上限；数组当前层数可能更小）。
    force_full: [[bool; MAX_IMAGE_PAGES as usize]; 2],
}

impl AtlasTextures {
    /// 建两张 1×1 占位数组；真实页在第一次上传前由 [`AtlasTextures::prepare`] 按页尺寸创建
    /// （懒分配，DEVIATIONS F-208）。
    pub(crate) fn new(device: &mut Device) -> Result<Self, EngineError> {
        Ok(Self {
            glyph: AtlasTexture::new(device, AtlasKind::Glyph)?,
            image: AtlasTexture::new(device, AtlasKind::Image)?,
            force_full: [[false; MAX_IMAGE_PAGES as usize]; 2],
        })
    }

    pub(crate) fn texture(&self, kind: AtlasKind) -> &ProtocolObject<dyn MTLTexture> {
        match kind {
            AtlasKind::Glyph => &self.glyph.texture,
            AtlasKind::Image => &self.image.texture,
        }
    }

    /// 登记一层的脏矩形上传（`pixels` 是 `rects` 顺序逐矩形紧密打包的字节，R06-3）。
    ///
    /// `layer` 是 texture array 的层号（字形恒 0；图片 = 图集页索引），`spec` 是该 array 的
    /// **运行时**规格（页边长 + 层数；图片层数 = 当前打开的图集页数）；层号硬断言按
    /// `spec.layers` 校验，不是 `MAX_IMAGE_PAGES` 常量。长度必须等于
    /// [`DirtyRects::byte_count`]（release 也硬断言：越界读会读穿堆）。
    pub(crate) fn queue_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        spec: crate::render::AtlasArraySpec,
        pixels: Vec<u8>,
        rects: DirtyRects,
    ) {
        assert!(
            u32::from(layer) < spec.layers,
            "atlas layer {layer} out of range for {kind:?}"
        );
        let expected = rects.byte_count(spec.page, texel_bytes(kind));
        assert_eq!(
            pixels.len(),
            expected,
            "atlas payload size mismatch for {kind:?} layer {layer}: expected {expected} bytes for the dirty rects of a {}² page, got {}",
            spec.page,
            pixels.len()
        );
        let texture = match kind {
            AtlasKind::Glyph => &mut self.glyph,
            AtlasKind::Image => &mut self.image,
        };
        texture.queue_upload(layer, spec, pixels, rects);
    }

    /// 本帧是否有待上传的矩形（任一层）。
    pub(crate) fn has_pending(&self) -> bool {
        self.glyph.first_pending().is_some() || self.image.first_pending().is_some()
    }

    /// 该图集是否有**任意层**的上传已入队、还没被命令缓冲消费（离屏目标的空闲判据）。
    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        match kind {
            AtlasKind::Glyph => self.glyph.first_pending().is_some(),
            AtlasKind::Image => self.image.first_pending().is_some(),
        }
    }

    /// 在编码本帧 blit **之前**把 array 规格变化落地（DEVIATIONS F-208）。
    ///
    /// 旧纹理可能在飞帧仍采样：新数组建好后把旧纹理句柄按本帧的 `sequence`（`u64` 提交序号）
    /// retire。绑定是每帧 `setFragmentTexture` 现场写的，不需要额外重写。
    ///
    /// 规格变化 = 页尺寸（字形扩容阶梯 / App 换代）或**层数增长**（图片图集开了新页；App 在
    /// 该帧对每个已打开页整页重传，因此重建后的数组内容全部由本帧的待上传层补齐 —— 增长不会
    /// 丢已上传的图）。重建之后本帧在该图集上的每个待上传层都必须是整页上传。
    pub(crate) fn prepare(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        sequence: u64,
    ) -> Result<AtlasRebuilt, EngineError> {
        let mut rebuilt = AtlasRebuilt::default();
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let texture = match kind {
                AtlasKind::Glyph => &mut self.glyph,
                AtlasKind::Image => &mut self.image,
            };
            let Some(spec) = texture.pending_spec() else {
                continue;
            };
            debug_assert!(
                texture
                    .pending
                    .iter()
                    .flatten()
                    .all(|pending| pending.spec.page == spec.page),
                "all layers of one atlas must share the same page extent"
            );
            if spec == texture.spec() {
                continue;
            }
            // fail-closed（RBT-01 不变量 2）：**扩层请求必须每一层都带整页上传**。重建后的数组
            // 内容未定义，只有「所有已打开层的整页内容都在同一次重传里入队」才允许扩张；否则
            // 这一帧直接拒绝（release 也生效，不是 debug_assert）。
            if spec.layers > texture.spec().layers {
                for layer in 0..spec.layers {
                    let full = texture
                        .pending
                        .get(layer as usize)
                        .and_then(Option::as_ref)
                        .is_some_and(|pending| {
                            pending.spec.page == spec.page && pending.rects.is_full()
                        });
                    if !full {
                        return Err(EngineError::AtlasGrowthIncomplete {
                            kind,
                            layer: u16::try_from(layer).unwrap_or(u16::MAX),
                            layers: spec.layers,
                        });
                    }
                }
            }
            texture.recreate(device, queue, sequence, spec)?;
            rebuilt.mark(kind);
            // 重建后的数组内容未定义：本帧该图集上的每个待上传层都必须整页上传。
            let mut force_layers = Vec::new();
            for (layer, pending) in texture.pending.iter().enumerate() {
                if pending.is_some() {
                    force_layers.push(layer);
                }
            }
            let force_full = &mut self.force_full[index];
            for layer in force_layers {
                force_full[layer] = true;
            }
        }
        Ok(rebuilt)
    }

    /// 把本帧待上传的**所有层**写进本帧的 staging 并编码 blit（必须在一帧的 command buffer 里
    /// 调用）。
    ///
    /// 小上传复用该帧槽的常驻缓冲（覆盖写安全：上一帧已完成）；超过 1 MiB 的上传临时分配一块
    /// 缓冲并以本帧的 `sequence`（`u64` 提交序号）进入 retire 队列（F-216），不会覆盖 GPU 还在读
    /// 的常驻缓冲。
    pub(crate) fn record_uploads(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        sequence: u64,
        slot: usize,
        encoder: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    ) -> Result<(), EngineError> {
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let force_full = std::mem::replace(
                &mut self.force_full[index],
                [false; MAX_IMAGE_PAGES as usize],
            );
            let texture = match kind {
                AtlasKind::Glyph => &mut self.glyph,
                AtlasKind::Image => &mut self.image,
            };
            texture.record_uploads(device, queue, sequence, slot, encoder, &force_full)?;
        }
        Ok(())
    }
}

impl AtlasTexture {
    /// 该数组的当前规格（页边长 + 层数）—— 上传器对齐 / 重建判据。
    fn spec(&self) -> crate::render::AtlasArraySpec {
        crate::render::AtlasArraySpec {
            page: self.page,
            layers: self.layers,
        }
    }

    fn new(device: &mut Device, kind: AtlasKind) -> Result<Self, EngineError> {
        let placeholder = crate::render::AtlasArraySpec {
            page: PLACEHOLDER_PAGE,
            layers: 1,
        };
        let texture = create_texture(device, kind, placeholder)?;
        Ok(Self {
            kind,
            layers: placeholder.layers,
            texture,
            staging: std::array::from_fn(|_| None),
            handle: None,
            page: placeholder.page,
            pending: (0..placeholder.layers as usize).map(|_| None).collect(),
        })
    }

    /// 任一层的待上传内容（`prepare` / 空闲判据用）。
    fn first_pending(&self) -> Option<&PendingUpload> {
        self.pending.iter().flatten().next()
    }

    /// 待上传层要求的 array 规格：页边长取自任一层（同一图集所有页等大），层数取**最大**的
    /// 待上传层（层数只增不减；入队层数 = App 当前打开的页数）。
    fn pending_spec(&self) -> Option<crate::render::AtlasArraySpec> {
        let first = self.first_pending()?;
        let layers = self
            .pending
            .iter()
            .flatten()
            .map(|pending| pending.spec.layers)
            .max()
            .unwrap_or(first.spec.layers);
        Some(crate::render::AtlasArraySpec {
            page: first.spec.page,
            layers,
        })
    }

    fn queue_upload(
        &mut self,
        layer: u16,
        spec: crate::render::AtlasArraySpec,
        pixels: Vec<u8>,
        rects: DirtyRects,
    ) {
        if rects.is_empty() {
            return;
        }
        // 待上传槽位表按运行时层数增长（新页刚打开的那一帧层数可能大于当前数组）。
        if self.pending.len() < spec.layers as usize {
            self.pending.resize_with(spec.layers as usize, || None);
        }
        let slot = &mut self.pending[usize::from(layer)];
        assert!(
            slot.is_none(),
            "atlas payload slot must be consumed before the next upload ({:?}, layer {layer})",
            self.kind
        );
        *slot = Some(PendingUpload {
            layer,
            spec,
            pixels,
            rects,
        });
    }

    /// 把**本帧该图集全部待上传层**写进本帧槽的 staging、逐矩形编码 buffer→array 层 blit。
    ///
    /// AOI（R06-3）：每个脏矩形一次 `copyFromBuffer`，`MTLSize` 与 `MTLOrigin` 都取矩形自己的
    /// `(w, h)` 与 `(x, y)`；staging 的行 pitch 是**矩形宽**（不是页宽）。D-9-c：同一帧的多层
    /// 聚合进**一块** staging，各层各自偏移（不能共用 offset 0，否则 GPU 只读到最后一层，与
    /// D3D12 V-64 同类），blit 的 `destinationSlice` 写层号。
    fn record_uploads(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        sequence: u64,
        slot: usize,
        encoder: &ProtocolObject<dyn MTLBlitCommandEncoder>,
        force_full: &[bool; MAX_IMAGE_PAGES as usize],
    ) -> Result<(), EngineError> {
        let pendings: Vec<PendingUpload> =
            self.pending.iter_mut().filter_map(Option::take).collect();
        if pendings.is_empty() {
            return Ok(());
        }
        debug_assert!(
            pendings
                .iter()
                .all(|pending| pending.spec.page == self.page),
            "page changes must be applied by AtlasTextures::prepare before recording"
        );
        debug_assert!(
            pendings
                .iter()
                .all(|pending| u32::from(pending.layer) < self.layers),
            "pending layers must be inside the texture array"
        );
        // 重建过纹理的那一帧，该层必须是整页上传（新纹理内容未定义）。
        // 覆盖整页的脏矩形等价于整页上传（哨兵与整页矩形都接受）。
        for pending in &pendings {
            assert!(
                !force_full[usize::from(pending.layer)] || pending.rects.covers_page(self.page),
                "a rebuilt atlas page must be uploaded as a full page (kind {:?}, layer {})",
                self.kind,
                pending.layer
            );
        }
        let texel = texel_bytes(self.kind) as usize;
        // staging 布局：每块起点与行 pitch 都按 [`METAL_STAGING_ALIGNMENT`] 对齐
        // （Metal 的 buffer→texture 拷要求行 pitch 是像素尺寸的倍数），块内行紧密排列。
        // 同一帧的多层按层号顺序排列、偏移连续（各层不共用 offset 0）。
        let mut layout = Vec::new();
        let mut length = 0usize;
        for (index, pending) in pendings.iter().enumerate() {
            for chunk in pending.rects.chunks(self.page, texel_bytes(self.kind)) {
                let row_pitch =
                    (chunk.width as usize * texel).next_multiple_of(METAL_STAGING_ALIGNMENT);
                let offset = length.next_multiple_of(METAL_STAGING_ALIGNMENT);
                length = offset + chunk.height as usize * row_pitch;
                layout.push(MetalRect {
                    pending: index,
                    layer: pending.layer,
                    chunk,
                    offset,
                    row_pitch,
                });
            }
        }
        if layout.is_empty() {
            // 空集合 = 零上传：不分配 staging、不编码 blit。
            return Ok(());
        }
        // 小上传复用每槽常驻 staging；整页级的大上传用一次性缓冲（本帧后 retire），
        // 常驻容量不随页尺寸放大（DEVIATIONS F-216）。
        let staging = if length as u64 <= MAX_RESIDENT_STAGING_BYTES {
            self.ensure_staging(device, queue, sequence, slot, length)?;
            self.staging[slot]
                .as_ref()
                .expect("ensure_staging must fill the slot")
                .buffer
                .clone()
        } else {
            let buffer = device
                .raw()
                .newBufferWithLength_options(length, MTLResourceOptions::StorageModeShared)
                .ok_or_else(|| {
                    init_error(
                        "newBufferWithLength_options(atlas transient)",
                        "returned nil",
                    )
                })?;
            let handle = device.allocate(GpuObject::Buffer(buffer.clone()));
            // 本帧命令缓冲还在读它：帧完成通知到达后再释放（I4）。
            queue.retire(sequence, handle);
            buffer
        };
        // **先**把全部层一次性写进 staging：同一帧的多层各有自己的偏移，后面的 CPU 写入不会
        // 覆盖前面层的字节（所有写入都发生在命令缓冲提交之前）。
        // SAFETY: staging 容量 = 全部块的 `offset + height × row_pitch`；源切片长度由
        // `queue_upload` 的 `byte_count` 断言保证，`chunk.offset` 是打包布局里的真实偏移。
        unsafe {
            let contents = staging.contents().as_ptr().cast::<u8>();
            for rect in &layout {
                let pending = &pendings[rect.pending];
                let width_bytes = rect.chunk.width as usize * texel;
                for row in 0..rect.chunk.height as usize {
                    std::ptr::copy_nonoverlapping(
                        pending
                            .pixels
                            .as_ptr()
                            .add(rect.chunk.offset + row * width_bytes),
                        contents.add(rect.offset + row * rect.row_pitch),
                        width_bytes,
                    );
                }
            }
        }
        // **再**逐矩形编码 blit：`destinationSlice` = texture array 的层号。
        // SAFETY: 源是本函数独占的 staging（容量覆盖本块），目标是本结构的纹理；尺寸与 origin 在
        // 两者边界内（`chunks` 已把矩形钳进页），层号由 `queue_upload` 保证在数组范围内。
        for rect in &layout {
            unsafe {
                encoder.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    &staging,
                    rect.offset,
                    rect.row_pitch,
                    rect.chunk.height as usize * rect.row_pitch,
                    MTLSize {
                        width: rect.chunk.width as usize,
                        height: rect.chunk.height as usize,
                        depth: 1,
                    },
                    &self.texture,
                    usize::from(rect.layer),
                    0,
                    MTLOrigin {
                        x: rect.chunk.x as usize,
                        y: rect.chunk.y as usize,
                        z: 0,
                    },
                );
            }
        }
        Ok(())
    }

    /// 保证第 `slot` 个 staging 至少 `bytes` 字节；扩容时旧缓冲按本帧的提交序号 retire。
    fn ensure_staging(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        sequence: u64,
        slot: usize,
        bytes: usize,
    ) -> Result<(), EngineError> {
        if self.staging[slot]
            .as_ref()
            .is_some_and(|staging| staging.capacity >= bytes)
        {
            return Ok(());
        }
        let buffer = device
            .raw()
            .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)
            .ok_or_else(|| {
                init_error("newBufferWithLength_options(atlas staging)", "returned nil")
            })?;
        // SAFETY: `MTLBuffer` 本身没有未定义初始化要求（Metal 不要求清零）。
        let handle = device.allocate(GpuObject::Buffer(buffer.clone()));
        let new = StagingBuffer {
            buffer,
            handle,
            capacity: bytes,
        };
        if let Some(old) = self.staging[slot].replace(new) {
            // 该槽的上一帧由 `Queue::begin` 的 semaphore 保证已完成；这里 retire 只是让句柄表
            // 走与其它资源相同的释放路径（I4）。
            queue.retire(sequence, old.handle);
        }
        Ok(())
    }

    /// 换 array：按新规格（页边长 + 层数）建新数组、旧纹理按本帧的提交序号 retire
    /// （可能在飞帧仍采样它）。
    fn recreate(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        sequence: u64,
        spec: crate::render::AtlasArraySpec,
    ) -> Result<(), EngineError> {
        let texture = create_texture(device, self.kind, spec)?;
        let handle = device.allocate(GpuObject::Texture(texture.clone()));
        let old_texture = std::mem::replace(&mut self.texture, texture);
        if let Some(old_handle) = self.handle.replace(handle) {
            queue.retire(sequence, old_handle);
        }
        drop(old_texture);
        self.page = spec.page;
        self.layers = spec.layers;
        Ok(())
    }
}

/// 建一张 `page × page`、`layers` 层的图集 texture array（占位页与真实页共用）。
fn create_texture(
    device: &Device,
    kind: AtlasKind,
    spec: crate::render::AtlasArraySpec,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, EngineError> {
    debug_assert!(
        spec.layers > 0,
        "texture array must have at least one layer"
    );
    let descriptor = MTLTextureDescriptor::new();
    descriptor.setTextureType(MTLTextureType::Type2DArray);
    descriptor.setPixelFormat(texture_format(kind));
    // SAFETY: 尺寸 / 层数 / mip 数来自运行时 array 规格（page ≥ 1，layers 1..=MAX_IMAGE_PAGES，
    // mip 恒 1），都在 Metal 的 texture array 合法范围内。
    unsafe {
        descriptor.setWidth(spec.page as usize);
        descriptor.setHeight(spec.page as usize);
        descriptor.setArrayLength(spec.layers as usize);
        descriptor.setMipmapLevelCount(1);
    }
    descriptor.setUsage(MTLTextureUsage::ShaderRead);
    descriptor.setStorageMode(MTLStorageMode::Private);
    device
        .raw()
        .newTextureWithDescriptor(&descriptor)
        .ok_or_else(|| init_error("newTextureWithDescriptor(atlas)", "returned nil"))
}

fn texture_format(kind: AtlasKind) -> MTLPixelFormat {
    match kind {
        // 字形是线性覆盖度，**非** sRGB（P07 §2.9）。
        AtlasKind::Glyph => MTLPixelFormat::R8Unorm,
        // 图片纹素是「linear 预乘 → sRGB 编码」，视图必须是 sRGB（contract §8）。
        AtlasKind::Image => MTLPixelFormat::BGRA8Unorm_sRGB,
    }
}

#[cfg(test)]
mod tests {
    use objc2_metal::{MTLCommandBuffer, MTLCommandEncoder};

    use super::AtlasTextures;
    use crate::{
        paint::atlas::AtlasKind,
        render::{
            atlas::DirtyRects,
            macos::{
                device::Device,
                queue::{InFlight, Queue},
                readback::{Readback, ReadbackImage},
            },
        },
        window::FrameIndex,
    };

    /// 小页（16²）够验证行落位；真实页 2048² 在单测里代价过高。
    const TEST_PAGE: u32 = 16;

    /// 图集 array 规格（测试页 + 层数）。
    fn spec(page: u32, layers: u32) -> crate::render::AtlasArraySpec {
        crate::render::AtlasArraySpec { page, layers }
    }

    /// 一张图集页的整页像素（BGRA8，行优先；初始全 0）。
    fn page() -> Vec<u8> {
        vec![0u8; (TEST_PAGE * TEST_PAGE) as usize * 4]
    }

    /// 往整页图里写一行 BGRA 颜色。
    fn fill_row(pixels: &mut [u8], row: u32, colour: [u8; 4]) {
        let start = row as usize * TEST_PAGE as usize * 4;
        for texel in pixels[start..start + TEST_PAGE as usize * 4]
            .as_chunks_mut::<4>()
            .0
        {
            *texel = colour;
        }
    }

    /// 取一行打包好的 BGRA 字节（`rows` 脏矩形的载荷）。
    fn row_pixels(pixels: &[u8], row: u32) -> Vec<u8> {
        let start = row as usize * TEST_PAGE as usize * 4;
        pixels[start..start + TEST_PAGE as usize * 4].to_vec()
    }

    /// 读回整页并取一行（`Readback` 做 BGRA→RGBA 交换）。
    fn read_row(image: &ReadbackImage, row: u32) -> [u8; 4] {
        let start = row as usize * TEST_PAGE as usize * 4;
        image.rgba8[start..start + 4].try_into().expect("4 bytes")
    }

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

    /// 整页哨兵（新窗口 / 页尺寸变化的载荷）。
    fn full_rects() -> DirtyRects {
        DirtyRects::full()
    }

    /// 录完当前全部待上传层并**同步提交**（单测每帧都等完成，读回内容才确定）。
    fn submit_frame(device: &mut Device, queue: &mut Queue, atlas: &mut AtlasTextures, index: u32) {
        let slot = queue.try_acquire().expect("upload slot");
        let command = queue.begin().expect("command buffer");
        atlas
            .prepare(device, queue, slot.signal_value)
            .expect("prepare");
        if atlas.has_pending() {
            let blit = command.blitCommandEncoder().expect("blit encoder");
            atlas
                .record_uploads(device, queue, slot.signal_value, slot.index, &blit)
                .expect("record");
            blit.endEncoding();
        }
        queue
            .submit_blocking(FrameIndex::from_raw(index), slot, &command, device)
            .expect("submit");
    }

    /// 读回图片图集的某一层（`layer` = texture array 层号）。
    fn read_layer(
        readback: &mut Readback,
        device: &Device,
        queue: &Queue,
        atlas: &AtlasTextures,
        layer: u16,
    ) -> ReadbackImage {
        readback
            .capture(
                device,
                queue,
                atlas.texture(AtlasKind::Image),
                (TEST_PAGE, TEST_PAGE),
                layer,
            )
            .expect("readback")
    }

    /// P1 回归：两帧在飞时各自持有自己的 staging。
    ///
    /// 旧实现是**单块**复用 staging：第 2 帧录制时的 CPU `memcpy` 会把第 1 帧还在等 GPU blit 的
    /// 字节覆盖掉（同一 offset 0），于是第 1 帧把第 2 帧的像素写进了自己的行。这里把两帧都**先
    /// 录制、再提交**（相当于第 1 帧在飞时第 2 帧开始录制），一次性 buffer 让两次上传互不干扰：
    /// 第 5 行必须是第 1 帧的颜色，第 7 行必须是第 2 帧的颜色。
    #[test]
    fn consecutive_uploads_do_not_share_staging_while_frames_are_in_flight() {
        let mut device = Device::new().expect("metal device");
        let mut queue = Queue::new(&device, std::sync::Arc::new(InFlight::new())).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");

        // 第 1 帧：layer 0 整页上传（新页必须整页，DEVIATIONS F-211），内容全零。
        atlas.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            page(),
            full_rects(),
        );
        let slot_full = queue.try_acquire().expect("upload slot full");
        let command_full = queue.begin().expect("command full");
        atlas
            .prepare(&mut device, &mut queue, slot_full.signal_value)
            .expect("prepare full");
        {
            let blit = command_full
                .blitCommandEncoder()
                .expect("blit encoder full");
            atlas
                .record_uploads(
                    &mut device,
                    &mut queue,
                    slot_full.signal_value,
                    slot_full.index,
                    &blit,
                )
                .expect("record full");
            blit.endEncoding();
        }
        queue
            .submit_blocking(
                FrameIndex::from_raw(1),
                slot_full,
                &command_full,
                &mut device,
            )
            .expect("submit full");
        assert_eq!(queue.retired_handles(), 0, "整页上传完成后 staging 已释放");

        // 第 2 帧：layer 0 的第 5 行 = 不透明深蓝（打包的 1 行）。
        let mut first_page = page();
        fill_row(&mut first_page, 5, [0x11, 0x22, 0x33, 0xFF]);
        let first_row = row_pixels(&first_page, 5);
        atlas.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            first_row,
            rows(5, 6),
        );
        let slot1 = queue.try_acquire().expect("upload slot 1");
        let command1 = queue.begin().expect("command 1");
        atlas
            .prepare(&mut device, &mut queue, slot1.signal_value)
            .expect("prepare frame 1");
        {
            let blit = command1.blitCommandEncoder().expect("blit encoder 1");
            atlas
                .record_uploads(
                    &mut device,
                    &mut queue,
                    slot1.signal_value,
                    slot1.index,
                    &blit,
                )
                .expect("record frame 1");
            blit.endEncoding();
        }
        // 第 3 帧：layer 0 的第 7 行 = 不透明橙。第 2 帧还没提交/完成。
        let mut second_page = page();
        fill_row(&mut second_page, 7, [0x44, 0x55, 0x66, 0xFF]);
        let second_row = row_pixels(&second_page, 7);
        atlas.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            second_row,
            rows(7, 8),
        );
        let slot2 = queue.try_acquire().expect("upload slot 2");
        let command2 = queue.begin().expect("command 2");
        atlas
            .prepare(&mut device, &mut queue, slot2.signal_value)
            .expect("prepare frame 2");
        {
            let blit = command2.blitCommandEncoder().expect("blit encoder 2");
            atlas
                .record_uploads(
                    &mut device,
                    &mut queue,
                    slot2.signal_value,
                    slot2.index,
                    &blit,
                )
                .expect("record frame 2");
            blit.endEncoding();
        }
        // 每槽常驻 staging（DEVIATIONS F-212）：复用不产生 retire，两帧在飞也不会互相覆盖。
        assert_eq!(
            queue.retired_handles(),
            0,
            "常驻 staging 按槽复用，不产生 retire 句柄"
        );
        queue
            .submit_blocking(FrameIndex::from_raw(2), slot1, &command1, &mut device)
            .expect("submit frame 1");
        queue
            .submit_blocking(FrameIndex::from_raw(3), slot2, &command2, &mut device)
            .expect("submit frame 2");
        assert_eq!(queue.retired_handles(), 0, "两帧完成后同样没有 retire 句柄");

        let mut readback = Readback::new();
        let image = read_layer(&mut readback, &device, &queue, &atlas, 0);
        assert_eq!(read_row(&image, 5), [0x33, 0x22, 0x11, 0xFF], "第 1 帧的行");
        assert_eq!(read_row(&image, 7), [0x66, 0x55, 0x44, 0xFF], "第 2 帧的行");
        assert_eq!(read_row(&image, 6), [0, 0, 0, 0], "未上传的行保持原样");
    }

    /// D-9-c：图片图集是多层 texture array（层数 = 打开的页数），layer 1 的上传只落 layer 1，layer 0 的读回不变。
    #[test]
    fn an_upload_lands_on_its_own_array_layer() {
        let mut device = Device::new().expect("metal device");
        let mut queue = Queue::new(&device, std::sync::Arc::new(InFlight::new())).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");

        // 第 1 帧：建 2 层数组（扩层要求**每一层**都有整页上传，RBT-01 不变量 2）：
        // layer 0 / layer 1 都整页上传全 0。
        atlas.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 2),
            page(),
            full_rects(),
        );
        atlas.queue_upload(
            AtlasKind::Image,
            1,
            spec(TEST_PAGE, 2),
            page(),
            full_rects(),
        );
        submit_frame(&mut device, &mut queue, &mut atlas, 1);
        assert_eq!(atlas.image.layers, 2, "2 层的 spec ⇒ 数组一开始就是 2 层");

        // 第 2 帧：layer 1 的第 5 行 = 不透明深蓝（数组已是 2 层，不触发扩层重建）。
        let mut second = page();
        fill_row(&mut second, 5, [0x11, 0x22, 0x33, 0xFF]);
        atlas.queue_upload(
            AtlasKind::Image,
            1,
            spec(TEST_PAGE, 2),
            row_pixels(&second, 5),
            rows(5, 6),
        );
        submit_frame(&mut device, &mut queue, &mut atlas, 2);

        let mut readback = Readback::new();
        let layer0 = read_layer(&mut readback, &device, &queue, &atlas, 0);
        assert_eq!(
            read_row(&layer0, 5),
            [0, 0, 0, 0],
            "layer 1 的上传不能写进 layer 0"
        );
        let layer1 = read_layer(&mut readback, &device, &queue, &atlas, 1);
        assert_eq!(
            read_row(&layer1, 5),
            [0x33, 0x22, 0x11, 0xFF],
            "layer 1 的上传必须落 layer 1"
        );
    }

    /// 页数 1 → 2（P2-B3 核心语义）：数组从 1 层重建为 **2 层**，且第 0 层内容仍在。
    ///
    /// 扩层帧 App 对每个已打开页整页重传（`App::upload_image_page` 的 `array_missing` 分支）。
    /// 变异：`prepare` 只比较页边长、忽略层数 ⇒ 不重建（数组仍 1 层）⇒ 层数断言失败。
    #[test]
    fn growing_the_image_array_keeps_the_existing_layer_content() {
        let mut device = Device::new().expect("metal device");
        let mut queue = Queue::new(&device, std::sync::Arc::new(InFlight::new())).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");
        const LAYER_ZERO: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];
        const LAYER_ONE: [u8; 4] = [0x44, 0x55, 0x66, 0xFF];
        const ZERO_RGBA: [u8; 4] = [LAYER_ZERO[2], LAYER_ZERO[1], LAYER_ZERO[0], LAYER_ZERO[3]];
        const ONE_RGBA: [u8; 4] = [LAYER_ONE[2], LAYER_ONE[1], LAYER_ONE[0], LAYER_ONE[3]];
        let fill = |colour: [u8; 4]| {
            let mut pixels = page();
            for texel in pixels.as_chunks_mut::<4>().0 {
                *texel = colour;
            }
            pixels
        };

        // 第 1 帧：1 层数组，layer 0 整页 = 颜色 A。
        atlas.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            fill(LAYER_ZERO),
            full_rects(),
        );
        submit_frame(&mut device, &mut queue, &mut atlas, 1);
        assert_eq!(atlas.image.layers, 1, "1 页 ⇒ 1 层");
        let mut readback = Readback::new();
        assert_eq!(
            read_row(&read_layer(&mut readback, &device, &queue, &atlas, 0), 0),
            ZERO_RGBA,
            "1 层数组的第 0 层内容"
        );

        // 第 2 帧：扩层到 2 层，同一帧把两个已打开页整页重传。
        atlas.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 2),
            fill(LAYER_ZERO),
            full_rects(),
        );
        atlas.queue_upload(
            AtlasKind::Image,
            1,
            spec(TEST_PAGE, 2),
            fill(LAYER_ONE),
            full_rects(),
        );
        submit_frame(&mut device, &mut queue, &mut atlas, 2);
        assert_eq!(
            atlas.image.layers, 2,
            "页数 1 → 2 ⇒ texture array 必须重建为 2 层"
        );
        assert_eq!(
            read_row(&read_layer(&mut readback, &device, &queue, &atlas, 0), 0),
            ZERO_RGBA,
            "重建后第 0 层内容必须仍在（增长不能丢已上传的图）"
        );
        assert_eq!(
            read_row(&read_layer(&mut readback, &device, &queue, &atlas, 1), 0),
            ONE_RGBA,
            "第 1 层（新页）内容必须正确"
        );
    }

    /// D-9-c：同一帧把 4 层聚合上传，各自落位（staging 偏移不能共用 0、层号不能串）。
    #[test]
    fn four_layers_uploaded_in_one_frame_each_land_on_their_layer() {
        let mut device = Device::new().expect("metal device");
        let mut queue = Queue::new(&device, std::sync::Arc::new(InFlight::new())).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");

        // 第 1 帧：4 层整页全 0（同一帧聚合；新数组的每个待上传层必须整页）。
        for layer in 0..4u16 {
            atlas.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 4),
                page(),
                full_rects(),
            );
        }
        submit_frame(&mut device, &mut queue, &mut atlas, 1);
        assert_eq!(
            atlas.image.layers, 4,
            "请求 4 层 ⇒ texture array 必须建成 4 层"
        );

        // 第 2 帧：4 层各写**不同行**（同一帧聚合，各层各自 staging 偏移）。
        let mut expected: Vec<(u16, u32, [u8; 4])> = Vec::with_capacity(4);
        for layer in 0..4u16 {
            let row = u32::from(layer) + 2;
            let colour = [0x10 + layer as u8, 0x20, 0x30, 0xFF];
            let mut pixels = page();
            fill_row(&mut pixels, row, colour);
            atlas.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 4),
                row_pixels(&pixels, row),
                rows(row, row + 1),
            );
            expected.push((layer, row, colour));
        }
        submit_frame(&mut device, &mut queue, &mut atlas, 2);

        let mut readback = Readback::new();
        let images: Vec<ReadbackImage> = (0..4u16)
            .map(|layer| read_layer(&mut readback, &device, &queue, &atlas, layer))
            .collect();
        for (layer, row, colour) in &expected {
            let image = &images[usize::from(*layer)];
            assert_eq!(
                read_row(image, *row),
                [colour[2], colour[1], colour[0], colour[3]],
                "layer {layer} 自己的行必须落位"
            );
            for (other, other_row, _) in &expected {
                if other != layer {
                    assert_eq!(
                        read_row(image, *other_row),
                        [0, 0, 0, 0],
                        "layer {other} 的行不能串进 layer {layer}"
                    );
                }
            }
        }
    }

    /// 载荷长度断言（DEVIATIONS F-211）：`pixels.len()` 必须等于 `行数 × page × texel`。
    #[test]
    #[should_panic(expected = "atlas payload size mismatch")]
    fn a_short_payload_is_rejected() {
        let mut device = Device::new().expect("metal device");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");
        let mut short = page();
        // 20 字节 ≠ 1 行 × 16 × 4。
        short.truncate(20);
        atlas.queue_upload(AtlasKind::Image, 0, spec(TEST_PAGE, 1), short, rows(0, 1));
    }
}

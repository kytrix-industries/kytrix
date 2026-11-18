//! 图集纹理：字形 `R8Unorm` + 图片 `BGRA8Unorm_sRGB`，脏行上传（`design/phase-12-macos-linux.md` §5.5）。
//!
//! 像素来源由上层持有（P05 §10.6 的定案：渲染层不保 CPU 副本）；本模块只做
//! 「staging 写 → blit 到纹理」与「generation 变化时整页重传」。
//!
//! **staging 是每 in-flight 槽一块常驻的 `MTLBuffer`**（DEVIATIONS F-212）：槽位复用由
//! `Queue::begin` 的三槽 permit 保证「上一帧已完成」，因此本帧可以直接覆盖写；容量不够时
//! 重建并把旧缓冲按提交序号 retire。上传是低频路径、每帧至多一次，常驻一块按需扩容的缓冲
//! 比每帧分配更省（也避免分配抖动）。超过 1 MiB 的上传（整页）改用一次性缓冲并按提交序号
//! retire（F-216），常驻容量因此不随页尺寸放大。
//!
//! retire 的键是 `u64` 提交序号（`FrameSlot::signal_value`），**不是** `FrameIndex`（R06-1）：
//! 帧序号会饱和，饱和后按它记账会提前释放只在飞帧仍在采样的纹理 / 缓冲。

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLDevice, MTLOrigin, MTLPixelFormat, MTLResourceOptions,
    MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        atlas::{AtlasRebuilt, DirtyRows, MAX_RESIDENT_STAGING_BYTES, texel_bytes},
        frame::MAX_FRAMES_IN_FLIGHT,
        macos::{
            device::{Device, GpuObject, init_error},
            queue::Queue,
        },
        retire::GpuResourceHandle,
    },
};

/// 初始化时的占位页（1×1）：让参数缓冲里的图集纹理在第一次上传之前也有效。
const PLACEHOLDER_PAGE: u32 = 1;

/// 一张图集页。
struct AtlasTexture {
    kind: AtlasKind,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    /// 每 in-flight 槽一块常驻 staging（DEVIATIONS F-212）：槽位复用由 `Queue::begin` 的
    /// 三槽 semaphore 保证「上一帧已完成」，因此可以覆盖写，无需按帧 retire。
    staging: [Option<StagingBuffer>; MAX_FRAMES_IN_FLIGHT],
    /// 占位页没有句柄（它从未进入任何帧）；真实页重建时旧句柄按提交序号 retire。
    handle: Option<GpuResourceHandle>,
    page: u32,
    pending: Option<PendingUpload>,
}

/// 每 in-flight 槽一块常驻 staging（`StorageModeShared`，CPU 直接写）。
struct StagingBuffer {
    buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
    handle: GpuResourceHandle,
    capacity: usize,
}

/// 待上传的行区间（一帧内多次 `queue_upload` 取并集）+ 该页的页尺寸。
#[derive(Debug)]
struct PendingUpload {
    page: u32,
    /// `rows` 顺序紧密打包的行（整页上传时就是整页）。
    pixels: Vec<u8>,
    rows: DirtyRows,
}

/// 两张页（字形 / 图片）。
pub(crate) struct AtlasTextures {
    glyph: AtlasTexture,
    image: AtlasTexture,
    /// `prepare` 重建过纹理的页：本帧必须整页上传（新纹理内容未定义）。
    force_full: [bool; 2],
}

impl AtlasTextures {
    /// 建两张 1×1 占位页；真实页在第一次上传前由 [`AtlasTextures::prepare`] 按像素长度创建
    /// （懒分配，DEVIATIONS F-208）。
    pub(crate) fn new(device: &mut Device) -> Result<Self, EngineError> {
        Ok(Self {
            glyph: AtlasTexture::new(device, AtlasKind::Glyph)?,
            image: AtlasTexture::new(device, AtlasKind::Image)?,
            force_full: [false, false],
        })
    }

    pub(crate) fn texture(&self, kind: AtlasKind) -> &ProtocolObject<dyn MTLTexture> {
        match kind {
            AtlasKind::Glyph => &self.glyph.texture,
            AtlasKind::Image => &self.image.texture,
        }
    }

    /// 登记一次脏行上传（`pixels` 是 `rows` 顺序紧密打包的行，DEVIATIONS F-211）。
    ///
    /// 长度必须等于 `行数 × page × texel`（release 也硬断言：越界读会读穿堆）。
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
        let texture = match kind {
            AtlasKind::Glyph => &mut self.glyph,
            AtlasKind::Image => &mut self.image,
        };
        texture.queue_upload(page, pixels, rows);
    }

    /// 本帧是否有待上传的行。
    pub(crate) fn has_pending(&self) -> bool {
        self.glyph.pending.is_some() || self.image.pending.is_some()
    }

    /// 该图集是否有一笔上传已入队、还没被命令缓冲消费（离屏目标的空闲判据）。
    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        match kind {
            AtlasKind::Glyph => self.glyph.pending.is_some(),
            AtlasKind::Image => self.image.pending.is_some(),
        }
    }

    /// 在编码本帧 blit **之前**把页尺寸变化落地（DEVIATIONS F-208）。
    ///
    /// 旧纹理可能在飞帧仍采样：新纹理建好后把旧纹理句柄按本帧的 `sequence`（`u64` 提交序号）
    /// retire。绑定是每帧 `setFragmentTexture` 现场写的，不需要额外重写。
    pub(crate) fn prepare(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        sequence: u64,
    ) -> Result<AtlasRebuilt, EngineError> {
        let mut rebuilt = AtlasRebuilt::default();
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let pending_index = match kind {
                AtlasKind::Glyph => 0,
                AtlasKind::Image => 1,
            };
            let texture = match kind {
                AtlasKind::Glyph => &mut self.glyph,
                AtlasKind::Image => &mut self.image,
            };
            let Some(page) = texture.pending.as_ref().map(|pending| pending.page) else {
                continue;
            };
            if page == texture.page {
                continue;
            }
            texture.recreate(device, queue, sequence, page)?;
            rebuilt.mark(kind);
            self.force_full[pending_index] = true;
        }
        Ok(rebuilt)
    }

    /// 把待上传的行写进本帧的 staging 并编码 blit（必须在一帧的 command buffer 里调用）。
    ///
    /// 小上传复用该帧槽的常驻缓冲（覆盖写安全：上一帧已完成）；超过 1 MiB 的整页上传临时
    /// 分配一块缓冲并以本帧的 `sequence`（`u64` 提交序号）进入 retire 队列（F-216），不会覆盖
    /// GPU 还在读的常驻缓冲。
    pub(crate) fn record_uploads(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        sequence: u64,
        slot: usize,
        encoder: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    ) -> Result<(), EngineError> {
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let pending_index = match kind {
                AtlasKind::Glyph => 0,
                AtlasKind::Image => 1,
            };
            let force_full = std::mem::take(&mut self.force_full[pending_index]);
            let texture = match kind {
                AtlasKind::Glyph => &mut self.glyph,
                AtlasKind::Image => &mut self.image,
            };
            texture.record_upload(device, queue, sequence, slot, encoder, kind, force_full)?;
        }
        Ok(())
    }
}

impl AtlasTexture {
    fn new(device: &mut Device, kind: AtlasKind) -> Result<Self, EngineError> {
        let texture = create_texture(device, kind, PLACEHOLDER_PAGE)?;
        Ok(Self {
            kind,
            texture,
            staging: std::array::from_fn(|_| None),
            handle: None,
            page: PLACEHOLDER_PAGE,
            pending: None,
        })
    }

    fn queue_upload(&mut self, page: u32, pixels: Vec<u8>, rows: DirtyRows) {
        if rows.spans().is_empty() {
            return;
        }
        assert!(
            self.pending.is_none(),
            "atlas payload slot must be consumed before the next upload ({:?})",
            self.kind
        );
        self.pending = Some(PendingUpload { page, pixels, rows });
    }

    /// 把打包好的脏行写进**本帧槽的常驻 staging**、逐段编码 buffer→texture blit。
    fn record_upload(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        sequence: u64,
        slot: usize,
        encoder: &ProtocolObject<dyn MTLBlitCommandEncoder>,
        kind: AtlasKind,
        force_full: bool,
    ) -> Result<(), EngineError> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        debug_assert_eq!(
            pending.page, self.page,
            "page changes must be applied by AtlasTextures::prepare before recording"
        );
        // 重建过纹理的那一帧必须是整页上传（新纹理内容未定义）。
        // 覆盖整页的脏行等价于整页上传（`mark(0, page)` 与 `mark(0, u32::MAX)` 都接受）。
        let covers_page =
            pending.rows.is_full() || pending.rows.spans() == std::slice::from_ref(&(0..self.page));
        assert!(
            !force_full || covers_page,
            "a rebuilt atlas page must be uploaded as a full page"
        );
        let full = 0..self.page;
        let spans: &[std::ops::Range<u32>] = if pending.rows.is_full() {
            std::slice::from_ref(&full)
        } else {
            pending.rows.spans()
        };
        let row_bytes = (self.page * texel_bytes(kind)) as usize;
        // 行必须落在页内：`pack_rows` / `row_count` 已按页钳过长度，这里若还越页说明打包
        // 缓冲与行描述不是同一份数据，按契约错误立刻失败（否则下面的 `ptr::copy` 会读到
        // `pending.pixels` 之外）。
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
        if total_rows == 0 {
            return Ok(());
        }
        // 小上传复用每槽常驻 staging；整页级的大上传用一次性缓冲（本帧后 retire），
        // 常驻容量不随页尺寸放大（DEVIATIONS F-216）。
        let length = total_rows as usize * row_bytes;
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
        let mut staging_row = 0u32;
        let mut packed_row = 0u32;
        for span in spans {
            let rows = span.end - span.start;
            let source = packed_row as usize * row_bytes;
            let destination = staging_row as usize * row_bytes;
            // SAFETY: staging 恰为全部段的总字节数；源切片长度由整页断言 + 页内行断言保证。
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pending.pixels.as_ptr().add(source),
                    staging.contents().as_ptr().cast::<u8>().add(destination),
                    rows as usize * row_bytes,
                );
            }
            // SAFETY: blit 的源是本函数独占的 staging（容量覆盖本段），目标是本结构的纹理；
            // 尺寸与 offset 在两者边界内。
            unsafe {
                encoder.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    &staging,
                    destination,
                    row_bytes,
                    rows as usize * row_bytes,
                    MTLSize {
                        width: self.page as usize,
                        height: rows as usize,
                        depth: 1,
                    },
                    &self.texture,
                    0,
                    0,
                    MTLOrigin {
                        x: 0,
                        y: span.start as usize,
                        z: 0,
                    },
                );
            }
            staging_row += rows;
            packed_row += rows;
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

    /// 换页：建新纹理、旧纹理按本帧的提交序号 retire（可能在飞帧仍采样它）。
    fn recreate(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        sequence: u64,
        page: u32,
    ) -> Result<(), EngineError> {
        let texture = create_texture(device, self.kind, page)?;
        let handle = device.allocate(GpuObject::Texture(texture.clone()));
        let old_texture = std::mem::replace(&mut self.texture, texture);
        if let Some(old_handle) = self.handle.replace(handle) {
            queue.retire(sequence, old_handle);
        }
        drop(old_texture);
        self.page = page;
        Ok(())
    }
}

/// 建一张 `page × page` 的图集纹理（占位页与真实页共用）。
fn create_texture(
    device: &Device,
    kind: AtlasKind,
    page: u32,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, EngineError> {
    let format = texture_format(kind);
    // SAFETY: 纯构造函数，参数是本模块给定的合法尺寸/格式。
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            format,
            page as usize,
            page as usize,
            false,
        )
    };
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
            atlas::DirtyRows,
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

    /// 读回整页并取一行（`Readback` 做 BGRA→RGBA 交换）。
    fn read_row(image: &ReadbackImage, row: u32) -> [u8; 4] {
        let start = row as usize * TEST_PAGE as usize * 4;
        image.rgba8[start..start + 4].try_into().expect("4 bytes")
    }

    fn rows(y0: u32, y1: u32) -> DirtyRows {
        let mut rows = DirtyRows::default();
        rows.mark(y0, y1);
        rows
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

        // 第 1 帧：整页上传（新页必须整页，DEVIATIONS F-211），内容全零。
        atlas.queue_upload(AtlasKind::Image, TEST_PAGE, page(), rows(0, u32::MAX));
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

        // 第 2 帧：第 5 行 = 不透明深蓝（打包的 1 行）。
        let mut first_page = page();
        fill_row(&mut first_page, 5, [0x11, 0x22, 0x33, 0xFF]);
        let first_row = first_page[5 * TEST_PAGE as usize * 4..6 * TEST_PAGE as usize * 4].to_vec();
        atlas.queue_upload(AtlasKind::Image, TEST_PAGE, first_row, rows(5, 6));
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
        // 第 3 帧：第 7 行 = 不透明橙。第 2 帧还没提交/完成。
        let mut second_page = page();
        fill_row(&mut second_page, 7, [0x44, 0x55, 0x66, 0xFF]);
        let second_row =
            second_page[7 * TEST_PAGE as usize * 4..8 * TEST_PAGE as usize * 4].to_vec();
        atlas.queue_upload(AtlasKind::Image, TEST_PAGE, second_row, rows(7, 8));
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
        let image = readback
            .capture(
                &device,
                &queue,
                atlas.texture(AtlasKind::Image),
                (TEST_PAGE, TEST_PAGE),
            )
            .expect("readback");
        assert_eq!(read_row(&image, 5), [0x33, 0x22, 0x11, 0xFF], "第 1 帧的行");
        assert_eq!(read_row(&image, 7), [0x66, 0x55, 0x44, 0xFF], "第 2 帧的行");
        assert_eq!(read_row(&image, 6), [0, 0, 0, 0], "未上传的行保持原样");
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
        atlas.queue_upload(AtlasKind::Image, TEST_PAGE, short, rows(0, 1));
    }
}

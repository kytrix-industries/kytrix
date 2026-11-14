//! 图集纹理：字形 `R8Unorm` + 图片 `BGRA8Unorm_sRGB`，脏行上传（`design/phase-12-macos-linux.md` §5.5）。
//!
//! 像素来源由上层持有（P05 §10.6 的定案：渲染层不保 CPU 副本）；本模块只做
//! 「staging 写 → blit 到纹理」与「generation 变化时整页重传」。
//!
//! **staging 是每帧一次性的 `MTLBuffer`**（Apple 的 short-lived upload 用法）：macOS 允许
//! [`crate::render::frame::MAX_FRAMES_IN_FLIGHT`] 帧在飞，复用同一块 staging 会让后一帧的 CPU
//! 写入覆盖前一帧 GPU blit 正在读的字节（Windows 用「每 in-flight 槽 staging + 槽 fence」解决
//! 同一个问题）。这里每次上传分配一块只装本次行区间的缓冲，编码完就按**本帧序号**进 retire
//! 队列，帧完成通知到达后才释放（I4）；上传是低频路径，分配的代价远小于一次整页 memcpy。

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLDevice, MTLOrigin, MTLPixelFormat, MTLResourceOptions,
    MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        atlas::DirtyRows,
        macos::{
            device::{Device, GpuObject, init_error},
            queue::Queue,
        },
    },
    window::FrameIndex,
};

/// 图集页边长（逻辑页 = 2048²；与 Windows 同值）。
pub(crate) const ATLAS_PAGE: u32 = 2048;

/// 一张图集页。
struct AtlasTexture {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    page: u32,
    texel_bytes: u32,
    pending: Option<PendingUpload>,
}

/// 待上传的行区间（一帧内多次 `queue_upload` 取并集）。
#[derive(Debug)]
struct PendingUpload {
    y0: u32,
    rows: u32,
    full: bool,
    pixels: Vec<u8>,
}

/// 两张页（字形 / 图片）。
pub(crate) struct AtlasTextures {
    glyph: AtlasTexture,
    image: AtlasTexture,
}

impl AtlasTextures {
    pub(crate) fn new(device: &Device, page: u32) -> Result<Self, EngineError> {
        Ok(Self {
            glyph: AtlasTexture::new(device, AtlasKind::Glyph, page)?,
            image: AtlasTexture::new(device, AtlasKind::Image, page)?,
        })
    }

    pub(crate) fn texture(&self, kind: AtlasKind) -> &ProtocolObject<dyn MTLTexture> {
        match kind {
            AtlasKind::Glyph => &self.glyph.texture,
            AtlasKind::Image => &self.image.texture,
        }
    }

    /// 登记一次脏行上传。
    ///
    /// `pixels` 按**所有权**移动进待上传槽：槽里已经是整页 `Vec<u8>`，再按切片复制一次会白搬
    /// 16 MiB（图片页）；与 Windows 的 `AtlasTextures::queue_upload` 同一约定。
    pub(crate) fn queue_upload(&mut self, kind: AtlasKind, pixels: Vec<u8>, rows: DirtyRows) {
        let texture = match kind {
            AtlasKind::Glyph => &mut self.glyph,
            AtlasKind::Image => &mut self.image,
        };
        let expected = (texture.page * texture.page) as usize * texture.texel_bytes as usize;
        // 始终生效的断言（与 Windows 同一约定）：`record_upload` 按页布局切出整段行，长度不足
        // 会读穿堆（`debug_assert_eq!` 只保护调试构建）。
        assert_eq!(
            pixels.len(),
            expected,
            "atlas page size mismatch for {kind:?}: expected {expected} bytes for a {}×{} page, got {}",
            texture.page,
            texture.page,
            pixels.len()
        );
        texture.queue_upload(pixels, rows);
    }

    /// 本帧是否有待上传的行。
    pub(crate) fn has_pending(&self) -> bool {
        self.glyph.pending.is_some() || self.image.pending.is_some()
    }

    /// 把待上传的行写进**本帧专用**的 staging 并编码 blit（必须在一帧的 command buffer 里调用）。
    ///
    /// 每页最多分配一块一次性缓冲；它们以 `index` 进入 retire 队列，因此 GPU 还在读的上一帧
    /// staging 不会被本帧的 CPU 写入覆盖。
    pub(crate) fn record_uploads(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        index: FrameIndex,
        encoder: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    ) -> Result<(), EngineError> {
        self.glyph.record_upload(device, queue, index, encoder)?;
        self.image.record_upload(device, queue, index, encoder)
    }
}

impl AtlasTexture {
    fn new(device: &Device, kind: AtlasKind, page: u32) -> Result<Self, EngineError> {
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
        let texture = device
            .raw()
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| init_error("newTextureWithDescriptor(atlas)", "returned nil"))?;
        Ok(Self {
            texture,
            page,
            texel_bytes: texel_bytes(kind),
            pending: None,
        })
    }

    fn queue_upload(&mut self, pixels: Vec<u8>, rows: DirtyRows) {
        let spans = rows.spans();
        if spans.is_empty() {
            return;
        }
        let full = rows.is_full();
        let first = spans.iter().map(|span| span.start).min().unwrap_or(0);
        let last = spans.iter().map(|span| span.end).max().unwrap_or(first);
        self.pending = Some(PendingUpload {
            y0: first,
            rows: last.saturating_sub(first),
            full,
            pixels,
        });
    }

    /// 分配本帧专用的 staging、拷入行区间、编码一段 buffer→texture blit，并把缓冲交给 retire。
    fn record_upload(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        index: FrameIndex,
        encoder: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    ) -> Result<(), EngineError> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        let row_bytes = (self.page * self.texel_bytes) as usize;
        let first = if pending.full { 0 } else { pending.y0 };
        let rows = if pending.full {
            self.page
        } else {
            pending.rows
        };
        if rows == 0 {
            return Ok(());
        }
        // 行区间必须落在页内（`DirtyRows` 的契约；越界读是 UB，因此是硬断言而不是 debug）。
        assert!(
            first.saturating_add(rows) <= self.page,
            "atlas upload rows {first}..{} exceed the {}-row page",
            first.saturating_add(rows),
            self.page
        );
        let length = rows as usize * row_bytes;
        let start = first as usize * row_bytes;
        // 一次性 staging：只装本次行区间（Apple 推荐的低频小上传方式）。容量不小于 1 字节
        // （Metal 拒绝 0 长度缓冲；`rows > 0` 已保证 length > 0）。
        let staging = device
            .raw()
            .newBufferWithLength_options(length, MTLResourceOptions::StorageModeShared)
            .ok_or_else(|| {
                init_error("newBufferWithLength_options(atlas staging)", "returned nil")
            })?;
        // SAFETY: staging 恰为 `length` 字节；源切片长度由整页断言 + 页内行断言保证。
        unsafe {
            std::ptr::copy_nonoverlapping(
                pending.pixels.as_ptr().add(start),
                staging.contents().as_ptr().cast::<u8>(),
                length,
            );
        }
        // SAFETY: blit 的源是本函数独占的 staging（长度 = `length`），目标是本结构的纹理；
        // 尺寸与 offset 在两者边界内。
        unsafe {
            encoder.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                &staging,
                0,
                row_bytes,
                length,
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
                    y: first as usize,
                    z: 0,
                },
            );
        }
        // GPU 读这块 staging 的 blit 与本帧命令缓冲同生命周期：帧完成（连续完成前沿到达
        // `index`）之前不得释放，因此这里只登记句柄，真正的 drop 由完成通知驱动（I4）。
        let handle = device.allocate(GpuObject::Buffer(staging));
        queue.retire(index, handle);
        Ok(())
    }
}

fn texture_format(kind: AtlasKind) -> MTLPixelFormat {
    match kind {
        // 字形是线性覆盖度，**非** sRGB（P07 §2.9）。
        AtlasKind::Glyph => MTLPixelFormat::R8Unorm,
        // 图片纹素是「linear 预乘 → sRGB 编码」，视图必须是 sRGB（contract §8）。
        AtlasKind::Image => MTLPixelFormat::BGRA8Unorm_sRGB,
    }
}

fn texel_bytes(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Glyph => 1,
        AtlasKind::Image => 4,
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
                queue::Queue,
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
        let mut queue = Queue::new(&device).expect("queue");
        let mut atlas = AtlasTextures::new(&device, TEST_PAGE).expect("atlas");

        // 第 1 帧：第 5 行 = 不透明深蓝。
        let mut first_page = page();
        fill_row(&mut first_page, 5, [0x11, 0x22, 0x33, 0xFF]);
        atlas.queue_upload(AtlasKind::Image, first_page, rows(5, 6));
        let (slot1, command1) = queue.begin().expect("command 1");
        {
            let blit = command1.blitCommandEncoder().expect("blit encoder 1");
            atlas
                .record_uploads(&mut device, &mut queue, FrameIndex::from_raw(1), &blit)
                .expect("record frame 1");
            blit.endEncoding();
        }
        // 第 2 帧：第 7 行 = 不透明橙。第 1 帧还没提交/完成。
        let mut second_page = page();
        fill_row(&mut second_page, 7, [0x44, 0x55, 0x66, 0xFF]);
        atlas.queue_upload(AtlasKind::Image, second_page, rows(7, 8));
        let (slot2, command2) = queue.begin().expect("command 2");
        {
            let blit = command2.blitCommandEncoder().expect("blit encoder 2");
            atlas
                .record_uploads(&mut device, &mut queue, FrameIndex::from_raw(2), &blit)
                .expect("record frame 2");
            blit.endEncoding();
        }
        assert_eq!(
            queue.retired_handles(),
            2,
            "每次上传各自一块一次性 staging，两帧都在飞时两块都不得释放"
        );

        // 第 1 帧完成：只释放它自己的 staging。
        queue
            .submit_blocking(FrameIndex::from_raw(1), slot1, &command1, &mut device)
            .expect("submit frame 1");
        assert_eq!(
            queue.retired_handles(),
            1,
            "第 2 帧的 staging 必须活到第 2 帧完成"
        );
        queue
            .submit_blocking(FrameIndex::from_raw(2), slot2, &command2, &mut device)
            .expect("submit frame 2");
        assert_eq!(queue.retired_handles(), 0, "两帧都完成后不得残留句柄");

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

    /// 页尺寸断言（与 Windows 同一约定）：长度不符的上传是上游契约破坏，不是可忽略的输入。
    #[test]
    #[should_panic(expected = "atlas page size mismatch")]
    fn an_upload_must_carry_a_full_page() {
        let device = Device::new().expect("metal device");
        let mut atlas = AtlasTextures::new(&device, TEST_PAGE).expect("atlas");
        let mut short = page();
        short.truncate(16);
        atlas.queue_upload(AtlasKind::Image, short, rows(0, 1));
    }
}

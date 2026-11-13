//! 图集纹理：字形 `R8Unorm` + 图片 `BGRA8Unorm_sRGB`，脏行上传（`design/phase-12-macos-linux.md` §5.5）。
//!
//! 像素来源由上层持有（P05 §10.6 的定案：渲染层不保 CPU 副本）；本模块只做
//! 「staging 写 → blit 到纹理」与「generation 变化时整页重传」。

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLDevice, MTLOrigin, MTLPixelFormat, MTLSize,
    MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        atlas::DirtyRows,
        macos::device::{Device, init_error},
    },
};

/// 图集页边长（逻辑页 = 2048²；与 Windows 同值）。
pub(crate) const ATLAS_PAGE: u32 = 2048;

/// staging 一次容纳的行数上限（超出则分段 blit）。
const STAGING_ROWS: u32 = 256;

/// 一张图集页。
struct AtlasTexture {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    staging: Retained<ProtocolObject<dyn MTLBuffer>>,
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
    pixels: Option<Vec<u8>>,
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

    /// 登记一次脏行上传（像素由调用方拥有，拷进来后由本模块负责写 staging）。
    pub(crate) fn queue_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        let texture = match kind {
            AtlasKind::Glyph => &mut self.glyph,
            AtlasKind::Image => &mut self.image,
        };
        texture.queue_upload(pixels, rows);
    }

    /// 本帧是否有待上传的行。
    pub(crate) fn has_pending(&self) -> bool {
        self.glyph.pending.is_some() || self.image.pending.is_some()
    }

    /// 把待上传的行写进 staging 并编码 blit（必须在一帧的 command buffer 里调用）。
    pub(crate) fn record_uploads(&mut self, encoder: &ProtocolObject<dyn MTLBlitCommandEncoder>) {
        self.glyph.record_upload(encoder);
        self.image.record_upload(encoder);
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
        let staging_bytes = (page * page * texel_bytes(kind)) as usize;
        let staging = device
            .raw()
            .newBufferWithLength_options(
                staging_bytes,
                objc2_metal::MTLResourceOptions::StorageModeShared,
            )
            .ok_or_else(|| init_error("newBufferWithLength_options(atlas)", "returned nil"))?;
        Ok(Self {
            texture,
            staging,
            page,
            texel_bytes: texel_bytes(kind),
            pending: None,
        })
    }

    fn queue_upload(&mut self, pixels: &[u8], rows: DirtyRows) {
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
            pixels: Some(pixels.to_vec()),
        });
    }

    fn record_upload(&mut self, encoder: &ProtocolObject<dyn MTLBlitCommandEncoder>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let Some(pixels) = pending.pixels else {
            return;
        };
        let row_bytes = (self.page * self.texel_bytes) as usize;
        let first = if pending.full { 0 } else { pending.y0 };
        let rows = if pending.full {
            self.page
        } else {
            pending.rows
        };
        let mut written = 0u32;
        while written < rows {
            let chunk = (rows - written).min(STAGING_ROWS);
            let offset = written as usize * row_bytes;
            let length = chunk as usize * row_bytes;
            let start = first as usize * row_bytes + offset;
            if start + length <= pixels.len() {
                // SAFETY: staging 容量 = page² × texel ≥ length + offset（chunk ≤ rows ≤ page），
                // 源切片长度足够（上方边界检查）。
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(start),
                        self.staging.contents().as_ptr().cast::<u8>().add(offset),
                        length,
                    );
                }
            }
            // SAFETY: blit 的源/目标是本结构独占的资源；尺寸与 offset 在 staging 与纹理边界内。
            unsafe {
                encoder.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    &self.staging,
                    offset,
                    row_bytes,
                    row_bytes * chunk as usize,
                    MTLSize {
                        width: self.page as usize,
                        height: chunk as usize,
                        depth: 1,
                    },
                    &self.texture,
                    0,
                    0,
                    MTLOrigin {
                        x: 0,
                        y: (first + written) as usize,
                        z: 0,
                    },
                );
            }
            written += chunk;
        }
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

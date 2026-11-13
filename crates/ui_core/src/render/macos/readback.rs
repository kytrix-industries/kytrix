//! 读回：persist（Private）→ blit 到 Shared buffer → `getBytes` 语义的 CPU 读取
//! （`design/phase-12-macos-linux.md` §5.7）。
//!
//! 字节语义与 D3D12 同契约：`BGRA8Unorm_sRGB` 的存储字节是 **sRGB 编码后的预乘值**，
//! 读回后只做 BGRA→RGBA 交换（不再做色彩转换）。仅测试 / `test-support` 构建存在。

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLDevice, MTLOrigin,
    MTLResourceOptions, MTLSize, MTLTexture,
};

use crate::{
    error::EngineError,
    render::macos::{
        device::{Device, init_error},
        queue::Queue,
    },
};

/// 读回的一帧像素（RGBA8，预乘，sRGB 编码字节）。
pub(crate) struct ReadbackImage {
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
}

/// 复用的 staging buffer（容量按 surface 的 `row_pitch * height`）。
pub(crate) struct Readback {
    staging: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    capacity: usize,
}

impl Readback {
    pub(crate) const fn new() -> Self {
        Self {
            staging: None,
            capacity: 0,
        }
    }

    /// persist(Private) → blit 到 Shared buffer → 逐行去 padding → BGRA→RGBA。
    pub(crate) fn capture(
        &mut self,
        device: &Device,
        queue: &Queue,
        texture: &ProtocolObject<dyn MTLTexture>,
        size: (u32, u32),
    ) -> Result<ReadbackImage, EngineError> {
        let row_bytes = size.0 as usize * 4;
        let total = row_bytes * size.1 as usize;
        if self.capacity < total || self.staging.is_none() {
            self.staging = device
                .raw()
                .newBufferWithLength_options(total.max(4), MTLResourceOptions::StorageModeShared);
            self.capacity = total;
        }
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| init_error("readback", "staging buffer allocation failed"))?;

        let buffer = queue.command_buffer()?;
        let blit = buffer
            .blitCommandEncoder()
            .ok_or_else(|| init_error("readback", "blitCommandEncoder returned nil"))?;
        // SAFETY: 源纹理与目标 buffer 都由本结构独占；尺寸与行 pitch 在两者容量内。
        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                texture,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
                MTLSize {
                    width: size.0 as usize,
                    height: size.1 as usize,
                    depth: 1,
                },
                staging,
                0,
                row_bytes,
                total,
            );
        }
        blit.endEncoding();
        buffer.commit();
        buffer.waitUntilCompleted();
        if buffer.status() == objc2_metal::MTLCommandBufferStatus::Error {
            return Err(init_error(
                "readback",
                buffer
                    .error()
                    .map(|error| error.localizedDescription().to_string())
                    .unwrap_or_else(|| "MTLCommandBuffer failed".to_owned()),
            ));
        }

        let mut rgba8 = vec![0u8; total];
        // SAFETY: staging 至少 total 字节（上方按 total 分配/扩容）；映射是 CPU 可见的 shared 内存。
        unsafe {
            std::ptr::copy_nonoverlapping(
                staging.contents().as_ptr().cast::<u8>(),
                rgba8.as_mut_ptr(),
                total,
            );
        }
        for pixel in rgba8.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Ok(ReadbackImage {
            width: size.0,
            height: size.1,
            rgba8,
        })
    }
}

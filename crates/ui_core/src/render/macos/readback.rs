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

pub(crate) struct ReadbackImage {
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
}

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

    pub(crate) fn capture(
        &mut self,
        device: &Device,
        queue: &Queue,
        texture: &ProtocolObject<dyn MTLTexture>,
        size: (u32, u32),
        layer: u16,
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
        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                texture,
                usize::from(layer),
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
        unsafe {
            std::ptr::copy_nonoverlapping(
                staging.contents().as_ptr().cast::<u8>(),
                rgba8.as_mut_ptr(),
                total,
            );
        }
        for pixel in rgba8.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Ok(ReadbackImage {
            width: size.0,
            height: size.1,
            rgba8,
        })
    }
}

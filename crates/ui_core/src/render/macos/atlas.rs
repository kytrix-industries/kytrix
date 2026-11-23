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

const PLACEHOLDER_PAGE: u32 = 1;

const METAL_STAGING_ALIGNMENT: usize = 4;

#[derive(Clone, Copy)]
struct MetalRect {
    pending: usize,
    layer: u16,
    chunk: RectChunk,
    offset: usize,
    row_pitch: usize,
}

struct AtlasTexture {
    kind: AtlasKind,
    layers: u32,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    staging: [Option<StagingBuffer>; MAX_FRAMES_IN_FLIGHT],
    handle: Option<GpuResourceHandle>,
    page: u32,
    pending: Vec<Option<PendingUpload>>,
}

struct StagingBuffer {
    buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
    handle: GpuResourceHandle,
    capacity: usize,
}

#[derive(Debug)]
struct PendingUpload {
    layer: u16,
    spec: crate::render::AtlasArraySpec,
    pixels: Vec<u8>,
    rects: DirtyRects,
}

pub(crate) struct AtlasTextures {
    glyph: AtlasTexture,
    image: AtlasTexture,
    force_full: [[bool; MAX_IMAGE_PAGES as usize]; 2],
}

impl AtlasTextures {
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

    pub(crate) fn has_pending(&self) -> bool {
        self.glyph.first_pending().is_some() || self.image.first_pending().is_some()
    }

    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        match kind {
            AtlasKind::Glyph => self.glyph.first_pending().is_some(),
            AtlasKind::Image => self.image.first_pending().is_some(),
        }
    }

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
            if spec.layers > texture.spec().layers {
                for layer in 0..spec.layers {
                    let full = texture
                        .pending
                        .get(layer as usize)
                        .and_then(Option::as_ref)
                        .is_some_and(|pending| {
                            pending.spec.page == spec.page && pending.rects.covers_page(spec.page)
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

    fn first_pending(&self) -> Option<&PendingUpload> {
        self.pending.iter().flatten().next()
    }

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
        for pending in &pendings {
            assert!(
                !force_full[usize::from(pending.layer)] || pending.rects.covers_page(self.page),
                "a rebuilt atlas page must be uploaded as a full page (kind {:?}, layer {})",
                self.kind,
                pending.layer
            );
        }
        let texel = texel_bytes(self.kind) as usize;
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
            return Ok(());
        }
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
            queue.retire(sequence, handle);
            buffer
        };
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
        let handle = device.allocate(GpuObject::Buffer(buffer.clone()));
        let new = StagingBuffer {
            buffer,
            handle,
            capacity: bytes,
        };
        if let Some(old) = self.staging[slot].replace(new) {
            queue.retire(sequence, old.handle);
        }
        Ok(())
    }

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
        AtlasKind::Glyph => MTLPixelFormat::R8Unorm,
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

    const TEST_PAGE: u32 = 16;

    fn spec(page: u32, layers: u32) -> crate::render::AtlasArraySpec {
        crate::render::AtlasArraySpec { page, layers }
    }

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

    fn row_pixels(pixels: &[u8], row: u32) -> Vec<u8> {
        let start = row as usize * TEST_PAGE as usize * 4;
        pixels[start..start + TEST_PAGE as usize * 4].to_vec()
    }

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

    fn full_rects() -> DirtyRects {
        DirtyRects::full()
    }

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

    #[test]
    fn consecutive_uploads_do_not_share_staging_while_frames_are_in_flight() {
        let mut device = Device::new().expect("metal device");
        let mut queue = Queue::new(&device, std::sync::Arc::new(InFlight::new())).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");

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

    #[test]
    fn an_upload_lands_on_its_own_array_layer() {
        let mut device = Device::new().expect("metal device");
        let mut queue = Queue::new(&device, std::sync::Arc::new(InFlight::new())).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");

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

    #[test]
    fn four_layers_uploaded_in_one_frame_each_land_on_their_layer() {
        let mut device = Device::new().expect("metal device");
        let mut queue = Queue::new(&device, std::sync::Arc::new(InFlight::new())).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");

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

    #[test]
    #[should_panic(expected = "atlas payload size mismatch")]
    fn a_short_payload_is_rejected() {
        let mut device = Device::new().expect("metal device");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");
        let mut short = page();
        short.truncate(20);
        atlas.queue_upload(AtlasKind::Image, 0, spec(TEST_PAGE, 1), short, rows(0, 1));
    }
}

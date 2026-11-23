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

const PLACEHOLDER_PAGE: u32 = 1;

struct SharedPage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    page: u32,
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
        let usage = vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST;
        #[cfg(test)]
        let usage = usage | vk::ImageUsageFlags::TRANSFER_SRC;
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
        let requirements = unsafe { device.raw().get_image_memory_requirements(image) };
        let Some(memory_type) =
            device.memory_type(requirements.memory_type_bits, MemoryNeed::DEVICE_IMAGE)
        else {
            unsafe { device.raw().destroy_image(image, None) };
            return Err(init_error(
                "atlas image memory",
                "no device-local memory type",
            ));
        };
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
                unsafe { device.raw().destroy_image(image, None) };
                return Err(init_error("allocate_memory(atlas)", format!("{error:?}")));
            }
        };
        if let Err(error) = unsafe { device.raw().bind_image_memory(image, memory, 0) } {
            unsafe {
                device.raw().destroy_image(image, None);
                device.raw().free_memory(memory, None);
            }
            return Err(init_error("bind_image_memory(atlas)", format!("{error:?}")));
        }
        let view = match unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
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

    fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

pub(crate) struct SharedAtlas {
    device: ash::Device,
    glyph: SharedPage,
    image: SharedPage,
    sampler: vk::Sampler,
}

impl SharedAtlas {
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

    pub(crate) fn image(&self, kind: AtlasKind) -> vk::Image {
        self.page_ref(kind).image
    }

    pub(crate) fn page(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).page
    }

    pub(crate) fn layers(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).layers
    }

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
        unsafe {
            self.device.destroy_sampler(self.sampler, None);
        }
        self.glyph.destroy(&self.device);
        self.image.destroy(&self.device);
    }
}

struct PendingUpload {
    layer: u16,
    spec: crate::render::AtlasArraySpec,
    queued_page: u32,
    pixels: Vec<u8>,
    rects: DirtyRects,
}

impl PendingUpload {
    fn covers_full_page(&self) -> bool {
        self.rects.covers_page(self.spec.page)
    }
}

pub(crate) struct AtlasUploads {
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

    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        self.pending[kind.index() as usize]
            .iter()
            .any(Option::is_some)
    }

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

    pub(crate) fn reconcile_pages(
        &mut self,
        current: [crate::render::AtlasArraySpec; 2],
    ) -> Result<Vec<AtlasPageRequest>, EngineError> {
        let mut rebuild: Vec<AtlasPageRequest> = Vec::with_capacity(2);
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let current = current[index];
            let pending_spec = self.pending[index]
                .iter()
                .flatten()
                .map(|pending| pending.spec)
                .fold(None, |acc: Option<crate::render::AtlasArraySpec>, spec| {
                    Some(match acc {
                        Some(acc) => crate::render::AtlasArraySpec {
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
                if pending.covers_full_page() && pending.queued_page != spec.page {
                    merge_page_request(&mut rebuild, kind, spec, current);
                } else {
                    self.pending[index][layer] = None;
                }
            }
        }
        Ok(rebuild)
    }
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

struct StagingRect {
    chunk: RectChunk,
    offset: u64,
    layer: u16,
    pending: usize,
}

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
        let layers = atlas.layers(kind);
        let texel = texel_bytes(kind);
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
            return Ok(());
        }
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
            .layer_count(layers);
        let to_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
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
                        .buffer_offset(rect.offset)
                        .buffer_row_length(0)
                        .buffer_image_height(0)
                        .image_offset(vk::Offset3D {
                            x: rect.chunk.x as i32,
                            y: rect.chunk.y as i32,
                            z: 0,
                        })
                        .image_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
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
            staging.retire(shared, retired);
        }
        Ok(())
    }

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

struct StagingRing {
    device: ash::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: u64,
    ptr: *mut u8,
}

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
        unsafe {
            self.device.unmap_memory(self.memory);
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

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

    fn retire(self, shared: &SharedGpu, retired: &mut Vec<GpuResourceHandle>) {
        unsafe { shared.raw().unmap_memory(self.memory) };
        retired.push(shared.allocate(GpuObject::Buffer(self.buffer)));
        retired.push(shared.allocate(GpuObject::Memory(self.memory)));
    }
}

fn create_host_staging(
    shared: &SharedGpu,
    bytes: u64,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8), EngineError> {
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
    let requirements = unsafe { shared.raw().get_buffer_memory_requirements(buffer) };
    let Some(memory_type) =
        shared.memory_type(requirements.memory_type_bits, MemoryNeed::HOST_WRITABLE)
    else {
        unsafe { shared.raw().destroy_buffer(buffer, None) };
        return Err(init_error(
            "atlas staging memory",
            "no host-visible memory type",
        ));
    };
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
            unsafe { shared.raw().destroy_buffer(buffer, None) };
            return Err(init_error(
                "allocate_memory(atlas staging)",
                format!("{error:?}"),
            ));
        }
    };
    if let Err(error) = unsafe { shared.raw().bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            shared.raw().destroy_buffer(buffer, None);
            shared.raw().free_memory(memory, None);
        }
        return Err(init_error(
            "bind_buffer_memory(atlas staging)",
            format!("{error:?}"),
        ));
    }
    let ptr = unsafe {
        shared
            .raw()
            .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
    }
    .map_err(|error| {
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
        AtlasKind::Glyph => vk::Format::R8_UNORM,
        AtlasKind::Image => vk::Format::B8G8R8A8_SRGB,
    }
}

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::AtlasUploads;
    use crate::{
        math::dp,
        paint::atlas::AtlasKind,
        render::{
            atlas::DirtyRects,
            linux::{
                device::{FrameSync, GpuObject, SharedGpu, gpu_test_lock},
                memory::MemoryNeed,
            },
        },
    };

    const TEST_PAGE: u32 = 16;

    fn full_rects() -> DirtyRects {
        DirtyRects::full()
    }

    fn rows(y0: u32, y1: u32) -> DirtyRects {
        let mut rects = DirtyRects::default();
        rects.mark(crate::math::DeviceRect::new(
            dp(0),
            dp(y0 as i32),
            dp(i32::MAX),
            dp(y1 as i32),
        ));
        rects
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

    fn command_pool(gpu: &SharedGpu) -> vk::CommandPool {
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
        unsafe { gpu.raw().end_command_buffer(command) }.expect("end command buffer");
    }

    fn submit_and_wait(gpu: &SharedGpu, command: vk::CommandBuffer) {
        let commands = [command];
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

    fn read_texel(
        gpu: &SharedGpu,
        pool: vk::CommandPool,
        image: vk::Image,
        layer: u16,
        x: u32,
        y: u32,
    ) -> [u8; 4] {
        let bytes = 4u64;
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

    fn spec(page: u32, layers: u32) -> crate::render::AtlasArraySpec {
        crate::render::AtlasArraySpec { page, layers }
    }

    fn request(kind: AtlasKind, page: u32, layers: u32) -> crate::render::AtlasPageRequest {
        crate::render::AtlasPageRequest {
            kind,
            spec: spec(page, layers),
        }
    }

    #[test]
    fn page_reconciliation_distinguishes_new_pages_from_stale_uploads() {
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
            uploads
                .reconcile_pages([spec(1, 1), spec(1, 1)])
                .expect("reconcile pages"),
            vec![request(AtlasKind::Glyph, 64, 1)]
        );
        assert!(uploads.pending(AtlasKind::Glyph), "新页要留到重建之后上传");

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
                .expect("reconcile pages")
                .is_empty()
        );
        assert!(!uploads.pending(AtlasKind::Glyph), "旧整页必须丢弃");

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
                .expect("reconcile pages")
                .is_empty()
        );
        assert!(!uploads.pending(AtlasKind::Glyph), "旧增量必须丢弃");

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
                .expect("reconcile pages")
                .is_empty()
        );
        assert!(uploads.pending(AtlasKind::Glyph), "页对齐的增量要保留");
    }

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
        assert!(
            (0..4).all(|layer| uploads.pending[1][layer].is_some()),
            "四层都要保留到重建之后上传"
        );
    }

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

    #[test]
    fn consecutive_uploads_do_not_share_staging_while_frames_are_in_flight() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut uploads = AtlasUploads::new();
        let pool = command_pool(&gpu);
        let commands = allocate(&gpu, pool, 2);

        uploads.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            gpu.atlas_page(AtlasKind::Image),
            page(),
            full_rects(),
        );
        let rebuild = uploads
            .reconcile_pages([
                gpu.atlas().spec(AtlasKind::Glyph),
                gpu.atlas().spec(AtlasKind::Image),
            ])
            .expect("reconcile pages");
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

        submit_and_wait(&gpu, commands[0]);
        assert_eq!(
            read_texel(&gpu, pool, image, 0, 0, 5),
            [0x11, 0x22, 0x33, 0xFF],
            "第 2 帧的 copy 必须读到自己 staging 的字节"
        );

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

        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

    #[test]
    fn an_aoi_rect_upload_only_writes_its_own_texels() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut uploads = AtlasUploads::new();
        let pool = command_pool(&gpu);
        let commands = allocate(&gpu, pool, 1);

        uploads.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            gpu.atlas_page(AtlasKind::Image),
            page(),
            full_rects(),
        );
        let rebuild = uploads
            .reconcile_pages([
                gpu.atlas().spec(AtlasKind::Glyph),
                gpu.atlas().spec(AtlasKind::Image),
            ])
            .expect("reconcile pages");
        gpu.rebuild_atlas_pages(&rebuild)
            .expect("rebuild image page");
        begin(&gpu, commands[0]);
        uploads
            .record_uploads(&gpu, &gpu.atlas(), 0, commands[0])
            .expect("record full");
        end(&gpu, commands[0]);
        submit_and_wait(&gpu, commands[0]);

        let mut rects = DirtyRects::default();
        rects.mark(crate::math::DeviceRect::new(dp(4), dp(5), dp(8), dp(6)));
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

        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

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
        let rebuild = uploads
            .reconcile_pages([
                gpu.atlas().spec(AtlasKind::Glyph),
                gpu.atlas().spec(AtlasKind::Image),
            ])
            .expect("reconcile pages");
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

        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

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

        uploads.queue_upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            gpu.atlas_page(AtlasKind::Image),
            fill(LAYER_ZERO),
            full_rects(),
        );
        let rebuild = uploads
            .reconcile_pages([
                gpu.atlas().spec(AtlasKind::Glyph),
                gpu.atlas().spec(AtlasKind::Image),
            ])
            .expect("reconcile pages");
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
        let rebuild = uploads
            .reconcile_pages([
                gpu.atlas().spec(AtlasKind::Glyph),
                gpu.atlas().spec(AtlasKind::Image),
            ])
            .expect("reconcile pages");
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

        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

    #[test]
    fn uploading_one_image_layer_preserves_the_other_layers() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut uploads = AtlasUploads::new();
        let pool = command_pool(&gpu);
        let commands = allocate(&gpu, pool, 1);
        const LAYER_ZERO: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];
        const LAYER_ONE: [u8; 4] = [0x44, 0x55, 0x66, 0xFF];

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
        uploads.queue_upload(
            AtlasKind::Image,
            1,
            spec(TEST_PAGE, 2),
            gpu.atlas_page(AtlasKind::Image),
            page(),
            full_rects(),
        );
        let rebuild = uploads
            .reconcile_pages([
                gpu.atlas().spec(AtlasKind::Glyph),
                gpu.atlas().spec(AtlasKind::Image),
            ])
            .expect("reconcile pages");
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

        unsafe { gpu.raw().destroy_command_pool(pool, None) };
    }

    #[test]
    fn teardown_drains_retired_resources_of_a_frame_that_never_submitted() {
        let _guard = gpu_test_lock();
        let gpu = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut sync = FrameSync::new(&gpu).expect("frame sync");
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

        assert_eq!(sync.drain_retire(6), 0);
        assert_eq!(gpu.live_resources(), 2);

        assert_eq!(sync.drain_all_retired(), 2);
        assert_eq!(sync.pending_retired(), 0);
        assert_eq!(gpu.live_resources(), 0);
    }
}

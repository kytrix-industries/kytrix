use ash::vk;

use crate::{
    error::EngineError,
    render::linux::{
        device::{SharedGpu, init_error},
        memory::MemoryNeed,
    },
};

pub(crate) struct ReadbackImage {
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
}

pub(crate) struct Readback {
    device: Option<ash::Device>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: u64,
}

impl Readback {
    pub(crate) fn new() -> Self {
        Self {
            device: None,
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            capacity: 0,
        }
    }

    pub(crate) fn record(
        &mut self,
        shared: &SharedGpu,
        command: vk::CommandBuffer,
        image: vk::Image,
        size: (u32, u32),
    ) -> Result<(), EngineError> {
        let bytes = u64::from(size.0) * u64::from(size.1) * 4;
        if self.capacity < bytes || self.buffer == vk::Buffer::null() {
            self.allocate(shared, bytes)?;
        }

        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let to_src = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
        unsafe {
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
            shared.raw().cmd_copy_image_to_buffer(
                command,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.buffer,
                &[vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: size.0,
                        height: size.1,
                        depth: 1,
                    })],
            );
            let back = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .image(image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[back],
            );
        }

        Ok(())
    }

    pub(crate) fn fetch(&mut self, size: (u32, u32)) -> Result<ReadbackImage, EngineError> {
        let bytes = u64::from(size.0) * u64::from(size.1) * 4;
        let Some(device) = self.device.as_ref() else {
            return Err(init_error("readback", "buffer is not allocated"));
        };
        let mut rgba8 = vec![0u8; bytes as usize];
        unsafe {
            let pointer = device
                .map_memory(self.memory, 0, bytes, vk::MemoryMapFlags::empty())
                .map_err(|error| init_error("map_memory(readback)", format!("{error:?}")))?;
            std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), rgba8.as_mut_ptr(), bytes as usize);
            device.unmap_memory(self.memory);
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

    fn allocate(&mut self, shared: &SharedGpu, bytes: u64) -> Result<(), EngineError> {
        self.release();
        unsafe {
            self.buffer = shared
                .raw()
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .map_err(|error| init_error("create_buffer(readback)", format!("{error:?}")))?;
            let requirements = shared.raw().get_buffer_memory_requirements(self.buffer);
            let memory_type = shared
                .memory_type(requirements.memory_type_bits, MemoryNeed::READBACK)
                .ok_or_else(|| init_error("readback memory", "no host-visible memory type"))?;
            self.memory = shared
                .raw()
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type),
                    None,
                )
                .map_err(|error| init_error("allocate_memory(readback)", format!("{error:?}")))?;
            shared
                .raw()
                .bind_buffer_memory(self.buffer, self.memory, 0)
                .map_err(|error| {
                    init_error("bind_buffer_memory(readback)", format!("{error:?}"))
                })?;
        }
        self.device = Some(shared.raw().clone());
        self.capacity = bytes;
        Ok(())
    }

    fn release(&mut self) {
        if self.buffer == vk::Buffer::null() {
            return;
        }
        let Some(device) = self.device.as_ref() else {
            return;
        };
        unsafe {
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
        self.buffer = vk::Buffer::null();
        self.memory = vk::DeviceMemory::null();
        self.capacity = 0;
    }
}

impl Drop for Readback {
    fn drop(&mut self) {
        self.release();
    }
}

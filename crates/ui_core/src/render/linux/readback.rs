//! 读回：persist（`*_SRGB` image）→ host-visible buffer → map（`design/phase-12-macos-linux.md` §6.7）。
//!
//! 字节语义与 D3D12 / Metal 同契约：存储字节是 **sRGB 编码后的预乘值**，读回后只做 BGRA→RGBA 交换。
//! 仅测试 / `test-support` 构建存在。

use ash::vk;

use crate::{
    error::EngineError,
    render::linux::device::{Device, init_error},
};

/// 读回的一帧像素（RGBA8，预乘，sRGB 编码字节）。
pub(crate) struct ReadbackImage {
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
}

/// 复用的 host-visible buffer。
pub(crate) struct Readback {
    /// 第一次 `capture` 时填（在此之前没有任何 Vulkan 句柄）。
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

    /// 把 image 拷进 host-visible buffer 并读回（调用方保证 image 处于
    /// `COLOR_ATTACHMENT_OPTIMAL`，且命令缓冲在 `capture` 之后立即提交 + 等待）。
    /// 只**录制** barrier + `cmd_copy_image_to_buffer`；映射必须在提交并 `queue_wait_idle`
    /// 之后由 [`Readback::fetch`] 做（否则读到的是提交前的旧内容）。
    pub(crate) fn record(
        &mut self,
        device: &Device,
        command: vk::CommandBuffer,
        image: vk::Image,
        size: (u32, u32),
    ) -> Result<(), EngineError> {
        let bytes = u64::from(size.0) * u64::from(size.1) * 4;
        if self.capacity < bytes || self.buffer == vk::Buffer::null() {
            self.allocate(device, bytes)?;
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
        // SAFETY: barrier / copy 的目标都是调用方持有的资源；范围在 image 边界内。
        unsafe {
            device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
            device.raw().cmd_copy_image_to_buffer(
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
            device.raw().cmd_pipeline_barrier(
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

    /// 提交并等待队列空闲之后的读取（map → 拷贝 → BGRA→RGBA 交换）。
    pub(crate) fn fetch(
        &mut self,
        device: &Device,
        size: (u32, u32),
    ) -> Result<ReadbackImage, EngineError> {
        let bytes = u64::from(size.0) * u64::from(size.1) * 4;
        let mut rgba8 = vec![0u8; bytes as usize];
        // SAFETY: buffer 至少 `bytes`（`record` 已按需扩容）；映射范围在分配内。
        unsafe {
            let pointer = device
                .raw()
                .map_memory(self.memory, 0, bytes, vk::MemoryMapFlags::empty())
                .map_err(|error| init_error("map_memory(readback)", format!("{error:?}")))?;
            std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), rgba8.as_mut_ptr(), bytes as usize);
            device.raw().unmap_memory(self.memory);
        }
        // 目标格式是 B8G8R8A8（或 R8G8B8A8）：统一换成交给 `SnapshotImage` 的 RGBA8。
        for pixel in rgba8.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Ok(ReadbackImage {
            width: size.0,
            height: size.1,
            rgba8,
        })
    }

    fn allocate(&mut self, device: &Device, bytes: u64) -> Result<(), EngineError> {
        self.release();
        // SAFETY: 句柄归本结构独占；失败时由 `release` 兜底。
        unsafe {
            self.buffer = device
                .raw()
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .map_err(|error| init_error("create_buffer(readback)", format!("{error:?}")))?;
            let requirements = device.raw().get_buffer_memory_requirements(self.buffer);
            let memory_type = device
                .memory_type(
                    requirements.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .ok_or_else(|| init_error("readback memory", "no host-visible memory type"))?;
            self.memory = device
                .raw()
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type),
                    None,
                )
                .map_err(|error| init_error("allocate_memory(readback)", format!("{error:?}")))?;
            device
                .raw()
                .bind_buffer_memory(self.buffer, self.memory, 0)
                .map_err(|error| {
                    init_error("bind_buffer_memory(readback)", format!("{error:?}"))
                })?;
        }
        self.device = Some(device.raw().clone());
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
        // SAFETY: 句柄归本结构独占；调用方保证 GPU 空闲。
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

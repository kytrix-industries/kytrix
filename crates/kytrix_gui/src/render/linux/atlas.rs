//! 图集纹理：`R8_UNORM` 字形 + `B8G8R8A8_SRGB` 图片，staging 上传（`design/phase-12-macos-linux.md` §6.5）。

use ash::vk;

use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        atlas::DirtyRows,
        linux::device::{Device, init_error},
    },
};

/// 图集页边长（与 Win32 / Metal 同值）。
pub(crate) const ATLAS_PAGE: u32 = 2048;

/// 一张图集页。
struct AtlasTexture {
    image: vk::Image,
    view: vk::ImageView,
    sampler: vk::Sampler,
    staging: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    page: u32,
    texel_bytes: u32,
    /// 首次上传前是 `UNDEFINED`，之后是 `SHADER_READ_ONLY_OPTIMAL`。
    initialized: bool,
    pending: Option<PendingUpload>,
}

/// 待上传的行（一帧内多次 `queue_upload` 取并集）。
struct PendingUpload {
    y0: u32,
    rows: u32,
    full: bool,
    pixels: Vec<u8>,
}

/// 两张页。
pub(crate) struct AtlasTextures {
    device: ash::Device,
    glyph: AtlasTexture,
    image: AtlasTexture,
}

impl AtlasTextures {
    pub(crate) fn new(device: &Device, page: u32) -> Result<Self, EngineError> {
        Ok(Self {
            device: device.raw().clone(),
            glyph: AtlasTexture::new(device, AtlasKind::Glyph, page)?,
            image: AtlasTexture::new(device, AtlasKind::Image, page)?,
        })
    }

    pub(crate) fn glyph_view(&self) -> vk::ImageView {
        self.glyph.view
    }

    pub(crate) fn image_view(&self) -> vk::ImageView {
        self.image.view
    }

    pub(crate) fn sampler(&self) -> vk::Sampler {
        self.glyph.sampler
    }

    pub(crate) fn queue_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        match kind {
            AtlasKind::Glyph => self.glyph.queue_upload(pixels, rows),
            AtlasKind::Image => self.image.queue_upload(pixels, rows),
        }
    }

    /// 本帧是否有待上传的行。
    pub(crate) fn has_pending(&self) -> bool {
        self.glyph.pending.is_some() || self.image.pending.is_some()
    }

    /// 把待上传的行写进 staging 并编码 `cmd_copy_buffer_to_image`（一帧的 command buffer 里）。
    pub(crate) fn record_uploads(&mut self, device: &Device, command: vk::CommandBuffer) {
        self.glyph.record_upload(device, command);
        self.image.record_upload(device, command);
    }
}

impl AtlasTexture {
    fn new(device: &Device, kind: AtlasKind, page: u32) -> Result<Self, EngineError> {
        let format = texture_format(kind);
        // SAFETY: 句柄归本结构独占；`Drop` 里销毁。
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
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .map_err(|error| init_error("create_image(atlas)", format!("{error:?}")))?;
        // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
        let requirements = unsafe { device.raw().get_image_memory_requirements(image) };
        let memory_type = device
            .memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .ok_or_else(|| init_error("atlas image memory", "no device-local memory type"))?;
        // SAFETY: `requirements` 来自本 image；`memory_type` 由 memory_type 查询保证合法。
        let memory = unsafe {
            device.raw().allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type),
                None,
            )
        }
        .map_err(|error| init_error("allocate_memory(atlas)", format!("{error:?}")))?;
        // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定/写入的范围在其分配内。
        unsafe { device.raw().bind_image_memory(image, memory, 0) }
            .map_err(|error| init_error("bind_image_memory(atlas)", format!("{error:?}")))?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let view = unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .map_err(|error| init_error("create_image_view(atlas)", format!("{error:?}")))?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let sampler = unsafe {
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
        }
        .map_err(|error| init_error("create_sampler(atlas)", format!("{error:?}")))?;

        let staging_bytes = u64::from(page) * u64::from(page) * u64::from(texel_bytes(kind));
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let staging = unsafe {
            device.raw().create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(staging_bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .map_err(|error| init_error("create_buffer(atlas staging)", format!("{error:?}")))?;
        // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
        let staging_requirements = unsafe { device.raw().get_buffer_memory_requirements(staging) };
        let staging_type = device
            .memory_type(
                staging_requirements.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
            .ok_or_else(|| init_error("atlas staging memory", "no host-visible memory type"))?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let staging_memory = unsafe {
            device.raw().allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(staging_requirements.size)
                    .memory_type_index(staging_type),
                None,
            )
        }
        .map_err(|error| init_error("allocate_memory(atlas staging)", format!("{error:?}")))?;
        // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定/写入的范围在其分配内。
        unsafe { device.raw().bind_buffer_memory(staging, staging_memory, 0) }.map_err(
            |error| init_error("bind_buffer_memory(atlas staging)", format!("{error:?}")),
        )?;

        Ok(Self {
            image,
            view,
            sampler,
            staging,
            staging_memory,
            page,
            texel_bytes: texel_bytes(kind),
            initialized: false,
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
            pixels: pixels.to_vec(),
        });
    }

    fn record_upload(&mut self, device: &Device, command: vk::CommandBuffer) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let row_bytes = (self.page * self.texel_bytes) as usize;
        let first = if pending.full { 0 } else { pending.y0 };
        let rows = if pending.full {
            self.page
        } else {
            pending.rows
        };
        let offset = first as usize * row_bytes;
        let length = rows as usize * row_bytes;
        if offset + length > pending.pixels.len() {
            return;
        }
        // SAFETY: staging 容量 = page² × texel ≥ offset + length；映射范围在缓冲内。
        unsafe {
            let pointer = device.raw().map_memory(
                self.staging_memory,
                0,
                vk::WHOLE_SIZE,
                vk::MemoryMapFlags::empty(),
            );
            if let Ok(pointer) = pointer {
                std::ptr::copy_nonoverlapping(
                    pending.pixels.as_ptr().add(offset),
                    pointer.cast::<u8>().add(offset),
                    length,
                );
                device.raw().unmap_memory(self.staging_memory);
            }
        }

        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let old_layout = if self.initialized {
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        } else {
            vk::ImageLayout::UNDEFINED
        };
        let to_dst = vk::ImageMemoryBarrier::default()
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(self.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        // SAFETY: barrier 与 copy 的目标都是本结构独占的资源，范围在图像边界内。
        unsafe {
            device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            device.raw().cmd_copy_buffer_to_image(
                command,
                self.staging,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy::default()
                    .buffer_offset(offset as u64)
                    .buffer_row_length(self.page)
                    .buffer_image_height(rows)
                    // 局部上传：目标从 `first` 行开始（缺这个字段会从第 0 行覆写整页）。
                    .image_offset(vk::Offset3D {
                        x: 0,
                        y: first as i32,
                        z: 0,
                    })
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: self.page,
                        height: rows,
                        depth: 1,
                    })],
            );
            let to_sampled = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(self.image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_sampled],
            );
        }
        self.initialized = true;
    }
}

impl Drop for AtlasTextures {
    fn drop(&mut self) {
        for texture in [&self.glyph, &self.image] {
            // SAFETY: 句柄归本结构独占；`Renderer::drop` 先 `wait_idle`。
            unsafe {
                self.device.destroy_sampler(texture.sampler, None);
                self.device.destroy_image_view(texture.view, None);
                self.device.destroy_image(texture.image, None);
                self.device.destroy_buffer(texture.staging, None);
                self.device.free_memory(texture.staging_memory, None);
            }
        }
    }
}

fn texture_format(kind: AtlasKind) -> vk::Format {
    match kind {
        // 字形是线性覆盖度，**非** sRGB（P07 §2.9）。
        AtlasKind::Glyph => vk::Format::R8_UNORM,
        // 图片纹素是「linear 预乘 → sRGB 编码」，视图必须是 sRGB（contract §8）。
        AtlasKind::Image => vk::Format::B8G8R8A8_SRGB,
    }
}

fn texel_bytes(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Glyph => 1,
        AtlasKind::Image => 4,
    }
}

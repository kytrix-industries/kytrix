//! 图集纹理：`R8_UNORM` 字形 + `B8G8R8A8_SRGB` 图片，staging 上传（`design/phase-12-macos-linux.md` §6.5）。
//!
//! **staging 是每次上传一次性的 `VkBuffer`**（与 macOS 本轮修复同一策略）：Linux 最多允许
//! [`MAX_FRAMES_IN_FLIGHT`](crate::render::frame::MAX_FRAMES_IN_FLIGHT) 帧在飞 —— 活性兜底会在
//! present 卡死时放行第二帧（这正是它存在的目的），所以不能假设「在飞 ≤ 1」。复用一块 staging
//! 会让后一帧录制时的 CPU 写入覆盖前一帧 GPU `cmd_copy_buffer_to_image` 仍在读的字节。这里每次
//! 上传只分配**本次脏行大小**的 host-visible 缓冲，录制时使用，随后按**本帧序号**进入 retire
//! 队列，由 timeline 完成推进释放（I4）。内存只随在飞帧的脏行增长（而不是固定按「页 × 槽位」
//! 放大），上传又是低频路径，分配的代价远小于一次整页 memcpy。

use ash::vk;

use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        atlas::{AtlasRebuilt, DirtyRows, texel_bytes},
        linux::device::{Device, GpuObject, init_error},
        retire::GpuResourceHandle,
    },
    window::FrameIndex,
};

/// 初始化时的占位页（1×1）：让描述符集合里的图集视图在第一次上传之前也有效。
const PLACEHOLDER_PAGE: u32 = 1;

/// 一张图集页。
struct AtlasTexture {
    kind: AtlasKind,
    image: vk::Image,
    /// image 的设备内存（与 `image` 成对释放）。
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
    page: u32,
    /// 首次上传前是 `UNDEFINED`，之后是 `SHADER_READ_ONLY_OPTIMAL`；换页后回到 `UNDEFINED`。
    initialized: bool,
    pending: Option<PendingUpload>,
}

/// 待上传的行（一帧内多次 `queue_upload` 取并集）+ 该页的页尺寸。
struct PendingUpload {
    page: u32,
    /// `rows` 顺序紧密打包的行（整页上传时就是整页）。
    pixels: Vec<u8>,
    rows: DirtyRows,
}

/// 两张页。
pub(crate) struct AtlasTextures {
    device: ash::Device,
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
            device: device.raw().clone(),
            glyph: AtlasTexture::new(device, AtlasKind::Glyph)?,
            image: AtlasTexture::new(device, AtlasKind::Image)?,
            force_full: [false, false],
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
        match kind {
            AtlasKind::Glyph => self.glyph.queue_upload(page, pixels, rows),
            AtlasKind::Image => self.image.queue_upload(page, pixels, rows),
        }
    }

    /// 本帧是否有待上传的行。
    pub(crate) fn has_pending(&self) -> bool {
        self.glyph.pending.is_some() || self.image.pending.is_some()
    }

    /// 在录制本帧**之前**把页尺寸变化落地（DEVIATIONS F-208）。
    ///
    /// 描述符集合会被重写（`Pipeline::write_atlas`），旧 image / view 也要销毁：两者都要求
    /// GPU 不再引用它们，因此这里先 `queue_wait_idle`。页变化只发生在首次使用与字形页换代
    /// （最多两档），等待代价可以接受。
    pub(crate) fn prepare(&mut self, device: &mut Device) -> Result<AtlasRebuilt, EngineError> {
        let mut rebuilt = AtlasRebuilt::default();
        let changes: [Option<u32>; 2] = [self.glyph.page_change(), self.image.page_change()];
        if changes.iter().all(Option::is_none) {
            return Ok(rebuilt);
        }
        // SAFETY: 设备 / 队列句柄由 `Device` 持有；等待期间不录制任何命令。
        unsafe { device.raw().queue_wait_idle(device.queue()) }
            .map_err(|error| init_error("queue_wait_idle(atlas page)", format!("{error:?}")))?;
        for (index, page) in changes.into_iter().enumerate() {
            let Some(page) = page else {
                continue;
            };
            let (kind, texture) = if index == 0 {
                (AtlasKind::Glyph, &mut self.glyph)
            } else {
                (AtlasKind::Image, &mut self.image)
            };
            texture.recreate(device, page)?;
            rebuilt.mark(kind);
            self.force_full[index] = true;
        }
        Ok(rebuilt)
    }

    /// 给待上传的行分配**本帧专用** staging、写进去并编码 `cmd_copy_buffer_to_image`。
    ///
    /// staging 以 `index` 进入 retire 队列：timeline 确认该帧完成之前，GPU 可能仍在读它，
    /// 因此即使下一帧立刻录制自己的上传也不会覆盖（每帧一块一次性缓冲，不共享）。
    pub(crate) fn record_uploads(
        &mut self,
        device: &mut Device,
        index: FrameIndex,
        command: vk::CommandBuffer,
    ) -> Result<(), EngineError> {
        let glyph_full = std::mem::take(&mut self.force_full[0]);
        self.glyph
            .record_upload(device, index, command, glyph_full)?;
        let image_full = std::mem::take(&mut self.force_full[1]);
        self.image
            .record_upload(device, index, command, image_full)?;
        Ok(())
    }
}

impl AtlasTexture {
    fn new(device: &mut Device, kind: AtlasKind) -> Result<Self, EngineError> {
        let (image, memory, view) = create_page(device, kind, PLACEHOLDER_PAGE)?;
        // SAFETY: 句柄归本结构独占；`Drop` 里销毁。
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
        Ok(Self {
            kind,
            image,
            memory,
            view,
            sampler,
            page: PLACEHOLDER_PAGE,
            initialized: false,
            pending: None,
        })
    }

    /// 入队一页的脏行（`pixels` 是 `rows` 顺序紧密打包的行，DEVIATIONS F-211）。
    fn queue_upload(&mut self, page: u32, pixels: Vec<u8>, rows: DirtyRows) {
        if rows.spans().is_empty() {
            return;
        }
        self.pending = Some(PendingUpload { page, pixels, rows });
    }

    /// 本帧待上传的页与当前纹理页不一致时的目标页（`None` = 不需要重建）。
    fn page_change(&self) -> Option<u32> {
        match &self.pending {
            Some(pending) if pending.page != self.page => Some(pending.page),
            _ => None,
        }
    }

    fn record_upload(
        &mut self,
        device: &mut Device,
        index: FrameIndex,
        command: vk::CommandBuffer,
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
        let row_bytes = (self.page * texel_bytes(self.kind)) as usize;
        let total_rows: u32 = spans.iter().map(|span| span.end - span.start).sum();
        if total_rows == 0 {
            return Ok(());
        }
        let length = total_rows as usize * row_bytes;
        // 一次性 staging 装本帧所有段；缓冲在 `index` 完成之前不得释放。
        let staging = OneShotStaging::new(device, length as u64)?;
        if let Err(error) = staging.write(device, 0, &pending.pixels, length) {
            // 这一帧不会再 submit（录制被错误中止）：它的序号永远不会出现在 timeline 上，
            // `drain_retire(last_index)` 到不了这里。仍按正常生命周期 retire，由 teardown 的
            // [`Device::drain_all_retired`] 兜底释放（map 失败 / 录制后失败 / device lost 同路）。
            staging.retire(device, index);
            return Err(error);
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
        // SAFETY: barrier 与 copy 的目标都是本结构独占的资源，范围在图像边界内；staging 是
        // 本次上传独占的缓冲（装全部段），行布局与 `image_extent` 一致（紧密排列）。
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
            let mut staging_row = 0u32;
            for span in spans {
                let rows = span.end - span.start;
                device.raw().cmd_copy_buffer_to_image(
                    command,
                    staging.buffer,
                    self.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy::default()
                        // 每段在 staging 里各有偏移（紧密排列的行）。
                        .buffer_offset(u64::from(staging_row) * row_bytes as u64)
                        .buffer_row_length(0)
                        .buffer_image_height(0)
                        // 局部上传：目标从 `span.start` 行开始（缺这个字段会从第 0 行覆写整页）。
                        .image_offset(vk::Offset3D {
                            x: 0,
                            y: span.start as i32,
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
                staging_row += rows;
            }
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
        // GPU 读这块 staging 的 copy 与本帧提交同生命周期：帧完成（timeline 的连续完成前沿
        // 到达 `index`）之前不得释放，因此只登记句柄（I4）。
        staging.retire(device, index);
        self.initialized = true;
        Ok(())
    }

    /// 换页：建新 image / memory / view；调用方已 `queue_wait_idle`，旧句柄立即销毁。
    fn recreate(&mut self, device: &mut Device, page: u32) -> Result<(), EngineError> {
        let (image, memory, view) = create_page(device, self.kind, page)?;
        let old_view = std::mem::replace(&mut self.view, view);
        let old_image = std::mem::replace(&mut self.image, image);
        let old_memory = std::mem::replace(&mut self.memory, memory);
        // SAFETY: GPU 已空闲（prepare 里 queue_wait_idle），句柄不再被任何在飞命令引用。
        // Vulkan 要求先销毁 view、再销毁 image、最后释放内存。
        unsafe {
            device.raw().destroy_image_view(old_view, None);
            device.raw().destroy_image(old_image, None);
            device.raw().free_memory(old_memory, None);
        }
        self.page = page;
        self.initialized = false;
        Ok(())
    }
}

/// 建一张 `page × page` 的图集页（image + 设备内存 + view；占位页与真实页共用）。
fn create_page(
    device: &mut Device,
    kind: AtlasKind,
    page: u32,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView), EngineError> {
    let format = texture_format(kind);
    // 产品路径只采样 / 上传，不读回图集；单测要验证「两帧各自的 staging 落到了自己的行」，
    // 因此 `cfg(test)` 下额外允许 `TRANSFER_SRC`（usage 是创建参数，测试无法事后追加）。
    let usage = vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST;
    #[cfg(test)]
    let usage = usage | vk::ImageUsageFlags::TRANSFER_SRC;
    // SAFETY: 句柄归调用方独占；`Drop` / retire 队列负责销毁。
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
                .usage(usage)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }
    .map_err(|error| init_error("create_image(atlas)", format!("{error:?}")))?;
    // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
    let requirements = unsafe { device.raw().get_image_memory_requirements(image) };
    let Some(memory_type) = device.memory_type(
        requirements.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    ) else {
        // SAFETY: image 已创建且尚未交给任何人。
        unsafe { device.raw().destroy_image(image, None) };
        return Err(init_error(
            "atlas image memory",
            "no device-local memory type",
        ));
    };
    // SAFETY: `requirements` 来自本 image；`memory_type` 由 memory_type 查询保证合法。
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
            // SAFETY: image 已创建且尚未交给任何人。
            unsafe { device.raw().destroy_image(image, None) };
            return Err(init_error("allocate_memory(atlas)", format!("{error:?}")));
        }
    };
    // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定/写入的范围在其分配内。
    if let Err(error) = unsafe { device.raw().bind_image_memory(image, memory, 0) } {
        // SAFETY: 两个句柄都还没有交给任何人。
        unsafe {
            device.raw().destroy_image(image, None);
            device.raw().free_memory(memory, None);
        }
        return Err(init_error("bind_image_memory(atlas)", format!("{error:?}")));
    }
    // SAFETY: 句柄与参数都由调用方独占，调用期间有效。
    let view = match unsafe {
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
    } {
        Ok(view) => view,
        Err(error) => {
            // SAFETY: 三个句柄都还没有交给任何人。
            unsafe {
                device.raw().destroy_image(image, None);
                device.raw().free_memory(memory, None);
            }
            return Err(init_error("create_image_view(atlas)", format!("{error:?}")));
        }
    };
    Ok((image, memory, view))
}

/// 一次上传专用的 staging（大小 = 本次行区间）。
///
/// 两个句柄以 `GpuResourceHandle` 的形式进入设备的 retire 队列：真正的 `vkDestroyBuffer` /
/// `vkFreeMemory` 由 timeline 完成推进触发，Atlas 不再持有跨帧的 staging 状态。
struct OneShotStaging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    buffer_handle: GpuResourceHandle,
    memory_handle: GpuResourceHandle,
}

impl OneShotStaging {
    fn new(device: &mut Device, bytes: u64) -> Result<Self, EngineError> {
        // SAFETY: 句柄归本结构独占；成功路径交给 retire 队列，失败路径在下面就地销毁。
        let buffer = unsafe {
            device.raw().create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .map_err(|error| init_error("create_buffer(atlas staging)", format!("{error:?}")))?;
        // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
        let requirements = unsafe { device.raw().get_buffer_memory_requirements(buffer) };
        let Some(memory_type) = device.memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        ) else {
            // SAFETY: buffer 已创建且尚未交给 retire 队列。
            unsafe { device.raw().destroy_buffer(buffer, None) };
            return Err(init_error(
                "atlas staging memory",
                "no host-visible memory type",
            ));
        };
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效。
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
                // SAFETY: buffer 已创建且尚未交给 retire 队列。
                unsafe { device.raw().destroy_buffer(buffer, None) };
                return Err(init_error(
                    "allocate_memory(atlas staging)",
                    format!("{error:?}"),
                ));
            }
        };
        // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定范围在其分配内。
        if let Err(error) = unsafe { device.raw().bind_buffer_memory(buffer, memory, 0) } {
            // SAFETY: 两个句柄都还没有进入 retire 队列。
            unsafe {
                device.raw().destroy_buffer(buffer, None);
                device.raw().free_memory(memory, None);
            }
            return Err(init_error(
                "bind_buffer_memory(atlas staging)",
                format!("{error:?}"),
            ));
        }
        let buffer_handle = device.allocate(GpuObject::Buffer(buffer));
        let memory_handle = device.allocate(GpuObject::Memory(memory));
        Ok(Self {
            buffer,
            memory,
            buffer_handle,
            memory_handle,
        })
    }

    /// 从 `pixels[offset..offset + length]` 拷进 staging（staging 容量恰为 `length`）。
    fn write(
        &self,
        device: &Device,
        offset: usize,
        pixels: &[u8],
        length: usize,
    ) -> Result<(), EngineError> {
        // SAFETY: staging 容量 = length；映射范围在分配内。
        unsafe {
            let pointer = device
                .raw()
                .map_memory(self.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .map_err(|error| init_error("map_memory(atlas staging)", format!("{error:?}")))?;
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr().add(offset),
                pointer.cast::<u8>(),
                length,
            );
            device.raw().unmap_memory(self.memory);
        }
        Ok(())
    }

    /// 本帧完成前不得释放：两个句柄按帧序号进入 retire 队列（先 buffer 后 memory）。
    fn retire(self, device: &mut Device, index: FrameIndex) {
        device.retire(index, self.buffer_handle);
        device.retire(index, self.memory_handle);
    }
}

impl Drop for AtlasTextures {
    fn drop(&mut self) {
        for texture in [&self.glyph, &self.image] {
            // SAFETY: 句柄归本结构独占；`Renderer::drop` 先 `wait_idle`。
            unsafe {
                self.device.destroy_sampler(texture.sampler, None);
                self.device.destroy_image_view(texture.view, None);
                // Vulkan 要求先销毁 image，再释放它的内存。
                self.device.destroy_image(texture.image, None);
                self.device.free_memory(texture.memory, None);
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

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::AtlasTextures;
    use crate::{
        paint::atlas::AtlasKind,
        render::{
            atlas::DirtyRows,
            linux::{device::Device, instance::VulkanInstance},
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

    fn rows(y0: u32, y1: u32) -> DirtyRows {
        let mut rows = DirtyRows::default();
        rows.mark(y0, y1);
        rows
    }

    fn command_pool(device: &Device) -> vk::CommandPool {
        // SAFETY: 池归测试独占；结束时销毁。
        unsafe {
            device
                .raw()
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(device.queue_family())
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .expect("command pool")
        }
    }

    fn allocate(device: &Device, pool: vk::CommandPool, count: u32) -> Vec<vk::CommandBuffer> {
        // SAFETY: 池由调用方持有，命令缓冲在提交完成后释放。
        unsafe {
            device
                .raw()
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(count),
                )
                .expect("command buffers")
        }
    }

    fn begin(device: &Device, command: vk::CommandBuffer) {
        // SAFETY: 命令缓冲由本测试独占，提交前只录制一次。
        unsafe {
            device
                .raw()
                .begin_command_buffer(
                    command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .expect("begin command buffer");
        }
    }

    fn end(device: &Device, command: vk::CommandBuffer) {
        // SAFETY: 录制收尾；随后提交并等待。
        unsafe {
            device
                .raw()
                .end_command_buffer(command)
                .expect("end command buffer");
        }
    }

    fn submit_and_wait(device: &Device, command: vk::CommandBuffer) {
        let commands = [command];
        // SAFETY: 命令缓冲已录制完毕；同步等待只发生在测试里。
        unsafe {
            device
                .raw()
                .queue_submit(
                    device.queue(),
                    &[vk::SubmitInfo::default().command_buffers(&commands)],
                    vk::Fence::null(),
                )
                .expect("queue submit");
            device
                .raw()
                .queue_wait_idle(device.queue())
                .expect("queue wait idle");
        }
    }

    /// 读回图集页某一行的前 4 字节（原始存储字节 = BGRA，不做通道交换）。
    fn read_row(device: &Device, pool: vk::CommandPool, image: vk::Image, row: u32) -> [u8; 4] {
        let bytes = u64::from(TEST_PAGE) * 4;
        // SAFETY: 句柄归本测试独占；提交并等待后读回，结束时销毁。
        unsafe {
            let buffer = device
                .raw()
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .expect("readback buffer");
            let requirements = device.raw().get_buffer_memory_requirements(buffer);
            let memory_type = device
                .memory_type(
                    requirements.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .expect("host-visible readback memory");
            let memory = device
                .raw()
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type),
                    None,
                )
                .expect("readback memory");
            device
                .raw()
                .bind_buffer_memory(buffer, memory, 0)
                .expect("bind readback memory");

            let command = allocate(device, pool, 1)[0];
            begin(device, command);
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            let to_src = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
            device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
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
                buffer,
                &[vk::BufferImageCopy::default()
                    .buffer_offset(0)
                    .image_offset(vk::Offset3D {
                        x: 0,
                        y: row as i32,
                        z: 0,
                    })
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: TEST_PAGE,
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
            device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[back],
            );
            end(device, command);
            submit_and_wait(device, command);

            let pointer = device
                .raw()
                .map_memory(memory, 0, bytes, vk::MemoryMapFlags::empty())
                .expect("map readback");
            let mut pixel = [0u8; 4];
            std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), pixel.as_mut_ptr(), 4);
            device.raw().unmap_memory(memory);
            device.raw().free_command_buffers(pool, &[command]);
            device.raw().destroy_buffer(buffer, None);
            device.raw().free_memory(memory, None);
            pixel
        }
    }

    /// P1 回归：两帧在飞时各自持有自己的 staging。
    ///
    /// 旧实现是**每张页一块**复用 staging：第 2 帧录制时的 CPU `memcpy` 会把第 1 帧还没提交
    /// 的 copy 要读的字节覆盖掉。这里让两帧上传**同一行**、都先录制再提交（第 1 帧在飞时第 2
    /// 帧开始录制）：旧实现下第 1 帧提交后读到的是第 2 帧的橙色（同一行、同一偏移），像素断言
    /// 直接失败；一次性 staging 下第 1 帧必须读到自己写入的深蓝，第 2 帧再把它覆盖成橙色。
    #[test]
    fn consecutive_uploads_do_not_share_staging_while_frames_are_in_flight() {
        let instance = VulkanInstance::new_headless().expect("headless Vulkan instance");
        let mut device = Device::new(&instance).expect("Vulkan device");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");
        let pool = command_pool(&device);
        let commands = allocate(&device, pool, 2);

        // 第 1 帧：第 5 行 = 不透明深蓝（BGRA）。先只录制，不提交。
        let mut first_page = page();
        fill_row(&mut first_page, 5, [0x11, 0x22, 0x33, 0xFF]);
        atlas.queue_upload(AtlasKind::Image, TEST_PAGE, first_page, rows(5, 6));
        begin(&device, commands[0]);
        atlas.prepare(&mut device).expect("prepare frame 1");
        atlas
            .record_uploads(&mut device, FrameIndex::from_raw(1), commands[0])
            .expect("record frame 1");
        end(&device, commands[0]);

        // 第 2 帧：**同一行** = 不透明橙。第 1 帧还没提交/完成：旧实现会在这里覆盖第 1 帧
        // staging 里的字节。
        let mut second_page = page();
        fill_row(&mut second_page, 5, [0x44, 0x55, 0x66, 0xFF]);
        atlas.queue_upload(AtlasKind::Image, TEST_PAGE, second_page, rows(5, 6));
        begin(&device, commands[1]);
        atlas.prepare(&mut device).expect("prepare frame 2");
        atlas
            .record_uploads(&mut device, FrameIndex::from_raw(2), commands[1])
            .expect("record frame 2");
        end(&device, commands[1]);

        // 第 1 帧完成：它写进第 5 行的必须是**自己的**深蓝，而不是第 2 帧的橙。旧实现（两张页
        // 共用一块 staging）会在这里失败 —— 像素先于任何 retire 计数断言暴露共享写入。
        submit_and_wait(&device, commands[0]);
        assert_eq!(
            read_row(&device, pool, atlas.image.image, 5),
            [0x11, 0x22, 0x33, 0xFF],
            "第 1 帧的 copy 必须读到自己 staging 的字节（旧实现读到的是第 2 帧覆盖后的橙）"
        );
        // 两帧各自的 staging（buffer + memory）都还在：提交完成 != retire 推进，第 2 帧也
        // 还没提交。
        assert_eq!(
            device.pending_retired(),
            4,
            "每次上传各自一块一次性 staging，两帧都在飞时都不得释放"
        );
        // 只释放第 1 帧自己的 staging：第 2 帧还没提交，完成前沿到不了它。
        assert_eq!(
            device.drain_retire(FrameIndex::from_raw(1)),
            2,
            "第 1 帧只释放自己的 buffer + memory"
        );
        assert_eq!(
            device.pending_retired(),
            2,
            "第 2 帧的 staging 必须活到第 2 帧完成"
        );

        // 第 2 帧完成：后一帧覆盖前一帧，之后再释放它的 staging。
        submit_and_wait(&device, commands[1]);
        assert_eq!(
            read_row(&device, pool, atlas.image.image, 5),
            [0x44, 0x55, 0x66, 0xFF],
            "后一帧的像素覆盖前一帧"
        );
        assert_eq!(device.drain_retire(FrameIndex::from_raw(2)), 2);
        assert_eq!(device.pending_retired(), 0, "两帧都完成后不得残留句柄");
        assert_eq!(
            read_row(&device, pool, atlas.image.image, 6),
            [0, 0, 0, 0],
            "未上传的行保持原样"
        );

        // SAFETY: 队列已空闲（每次提交都 wait idle），命令池没有在飞的缓冲。
        unsafe { device.raw().destroy_command_pool(pool, None) };
    }

    /// P2 回归：未提交的帧 retire 的 staging 由 teardown drain 释放。
    ///
    /// `map_memory` 失败无法注入，但失败路径的后果与这里一致：`record_upload` 先把 staging
    /// `retire(index)` 再返回 `Err`，而该帧永远不会 submit ⇒ timeline 永远到不了这个序号。
    /// 旧的 teardown（`drain_retire(last_index)`）只推进到最后一个**成功提交**的帧，放不掉它：
    /// 两个句柄留在 `ResourceTable` 里直到 `vkDestroyDevice`。`Device::drop` 现在先
    /// [`Device::drain_all_retired`]，覆盖 map 失败 / 录制后失败 / device lost 三条错误路径。
    #[test]
    fn teardown_drain_releases_retired_staging_of_a_frame_that_never_submitted() {
        let instance = VulkanInstance::new_headless().expect("headless Vulkan instance");
        let mut device = Device::new(&instance).expect("Vulkan device");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");
        let pool = command_pool(&device);
        let commands = allocate(&device, pool, 2);

        // 第 1 帧真正提交并完成：`last_index` 推进到 1。
        let mut first_page = page();
        fill_row(&mut first_page, 5, [0x11, 0x22, 0x33, 0xFF]);
        atlas.queue_upload(AtlasKind::Image, TEST_PAGE, first_page, rows(5, 6));
        begin(&device, commands[0]);
        atlas.prepare(&mut device).expect("prepare frame 1");
        atlas
            .record_uploads(&mut device, FrameIndex::from_raw(1), commands[0])
            .expect("record frame 1");
        end(&device, commands[0]);
        submit_and_wait(&device, commands[0]);
        assert_eq!(device.drain_retire(FrameIndex::from_raw(1)), 2);

        // 第 2 帧：staging 已 retire 在序号 2，但录制随后的失败让这一帧从未 submit。
        let mut second_page = page();
        fill_row(&mut second_page, 5, [0x44, 0x55, 0x66, 0xFF]);
        atlas.queue_upload(AtlasKind::Image, TEST_PAGE, second_page, rows(5, 6));
        begin(&device, commands[1]);
        atlas.prepare(&mut device).expect("prepare frame 2");
        atlas
            .record_uploads(&mut device, FrameIndex::from_raw(2), commands[1])
            .expect("record frame 2");
        end(&device, commands[1]);
        assert_eq!(device.pending_retired(), 2);

        // 旧 teardown 只 drain 到 `last_index`（= 1）：这两个句柄留在表里。
        assert_eq!(
            device.drain_retire(FrameIndex::from_raw(1)),
            0,
            "未提交的序号不参与完成前沿"
        );
        assert_eq!(
            device.pending_retired(),
            2,
            "旧 teardown 路径会把它们留到 vkDestroyDevice"
        );

        // 新的 teardown drain：设备销毁前把剩下的句柄全部真正释放。
        assert_eq!(
            device.drain_all_retired(),
            2,
            "teardown 必须销毁未提交帧的 buffer + memory"
        );
        assert_eq!(device.pending_retired(), 0, "teardown 之后不得残留句柄");
        assert_eq!(
            read_row(&device, pool, atlas.image.image, 5),
            [0x11, 0x22, 0x33, 0xFF],
            "未提交的帧没有改动图集：第 5 行仍是第 1 帧的深蓝"
        );

        // SAFETY: 队列已空闲（每次提交都 wait idle），命令池没有在飞的缓冲。
        unsafe { device.raw().destroy_command_pool(pool, None) };
    }
}

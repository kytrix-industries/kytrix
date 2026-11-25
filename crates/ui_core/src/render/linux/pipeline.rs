use std::path::PathBuf;

use ash::vk;

use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        geometry::{GeometryPipeline, Topology},
    },
    render::{
        frame::MAX_FRAMES_IN_FLIGHT,
        linux::device::{Device, init_error},
    },
};

pub(crate) const ROOT_UNIFORM_BYTES: u32 = 112;

pub(crate) const RENDER_FORMAT: vk::Format = vk::Format::B8G8R8A8_SRGB;

pub(crate) const MAX_WINDOWS: u32 = 64;

const BINDING_INSTANCES: u32 = 0;
const BINDING_CLIPS: u32 = 1;
const BINDING_GLYPH_ATLAS: u32 = 4;
const BINDING_IMAGE_ATLAS: u32 = 5;
const BINDING_COLOR_GLYPH_ATLAS: u32 = 6;

const QUAD_VS: &str = concat!(env!("OUT_DIR"), "/shaders/quad.vs.spv");
const QUAD_PS: &str = concat!(env!("OUT_DIR"), "/shaders/quad.ps.spv");
const SPIRV_MAGIC: [u8; 4] = [0x03, 0x02, 0x23, 0x07];
const SKIPPED: &[u8] = b"KYTRIX-SHADER-SKIPPED";

#[derive(Clone, Copy)]
struct GeometryPipelineSet {
    ui: vk::Pipeline,
    image: vk::Pipeline,
    glyph: vk::Pipeline,
}

impl GeometryPipelineSet {
    fn get(self, pipeline: GeometryPipeline) -> vk::Pipeline {
        match pipeline {
            GeometryPipeline::Ui => self.ui,
            GeometryPipeline::Textured {
                atlas: AtlasKind::Image,
            } => self.image,
            GeometryPipeline::Textured {
                atlas: AtlasKind::Glyph | AtlasKind::ColorGlyph,
            } => self.glyph,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct GeometryPipelines {
    list: GeometryPipelineSet,
    strip: GeometryPipelineSet,
}

impl GeometryPipelines {
    pub(crate) fn get(self, pipeline: GeometryPipeline, topology: Topology) -> vk::Pipeline {
        match topology {
            Topology::TriangleList => self.list.get(pipeline),
            Topology::TriangleStrip => self.strip.get(pipeline),
        }
    }
}

pub(crate) struct Pipeline {
    device: ash::Device,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pass_clear: vk::RenderPass,
    pass_load: vk::RenderPass,
    quad: vk::Pipeline,
    geometry: GeometryPipelines,
    cache: PipelineCache,
}

impl Pipeline {
    pub(crate) fn new(device: &Device) -> Result<Self, EngineError> {
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(BINDING_INSTANCES)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(BINDING_CLIPS)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(BINDING_GLYPH_ATLAS)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(BINDING_IMAGE_ATLAS)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(BINDING_COLOR_GLYPH_ATLAS)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let set_layout = unsafe {
            device.raw().create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .map_err(|error| init_error("create_descriptor_set_layout", format!("{error:?}")))?;

        let set_layouts = [set_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(ROOT_UNIFORM_BYTES)];
        let layout = unsafe {
            device.raw().create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push),
                None,
            )
        }
        .map_err(|error| init_error("create_pipeline_layout", format!("{error:?}")))?;

        let cache = PipelineCache::new(device);

        let pass_clear = create_render_pass(
            device.raw(),
            vk::AttachmentLoadOp::CLEAR,
            vk::ImageLayout::UNDEFINED,
        )?;
        let pass_load = create_render_pass(
            device.raw(),
            vk::AttachmentLoadOp::LOAD,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        )?;
        let quad = create_quad_pipeline(device.raw(), cache.raw(), layout, pass_clear)?;
        let create_set =
            |topology: vk::PrimitiveTopology| -> Result<GeometryPipelineSet, EngineError> {
                Ok(GeometryPipelineSet {
                    ui: create_geometry_pipeline(
                        device.raw(),
                        cache.raw(),
                        layout,
                        pass_clear,
                        "geometry.vs_ui.spv",
                        "geometry.ps_ui.spv",
                        12,
                        topology,
                    )?,
                    image: create_geometry_pipeline(
                        device.raw(),
                        cache.raw(),
                        layout,
                        pass_clear,
                        "geometry.vs_textured.spv",
                        "geometry.ps_textured_image.spv",
                        16,
                        topology,
                    )?,
                    glyph: create_geometry_pipeline(
                        device.raw(),
                        cache.raw(),
                        layout,
                        pass_clear,
                        "geometry.vs_textured.spv",
                        "geometry.ps_textured_glyph.spv",
                        16,
                        topology,
                    )?,
                })
            };
        let geometry = GeometryPipelines {
            list: create_set(vk::PrimitiveTopology::TRIANGLE_LIST)?,
            strip: create_set(vk::PrimitiveTopology::TRIANGLE_STRIP)?,
        };

        Ok(Self {
            device: device.raw().clone(),
            cache,
            set_layout,
            layout,
            pass_clear,
            pass_load,
            quad,
            geometry,
        })
    }

    pub(crate) fn set_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }

    pub(crate) fn geometry(&self, pipeline: GeometryPipeline, topology: Topology) -> vk::Pipeline {
        self.geometry.get(pipeline, topology)
    }

    pub(crate) fn render_pass(&self, clear: bool) -> vk::RenderPass {
        if clear {
            self.pass_clear
        } else {
            self.pass_load
        }
    }

    pub(crate) fn layout(&self) -> vk::PipelineLayout {
        self.layout
    }

    pub(crate) fn quad(&self) -> vk::Pipeline {
        self.quad
    }

    pub(crate) fn write_slot(&self, set: vk::DescriptorSet, upload: vk::Buffer, clip_offset: u64) {
        let instance_info = [vk::DescriptorBufferInfo::default()
            .buffer(upload)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let clip_info = [vk::DescriptorBufferInfo::default()
            .buffer(upload)
            .offset(clip_offset)
            .range(vk::WHOLE_SIZE)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_INSTANCES)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&instance_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_CLIPS)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&clip_info),
        ];
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
    }

    pub(crate) fn write_atlas(
        &self,
        sets: impl Iterator<Item = vk::DescriptorSet>,
        glyph: vk::ImageView,
        image: vk::ImageView,
        color_glyph: vk::ImageView,
        sampler: vk::Sampler,
    ) {
        let glyph_info = [vk::DescriptorImageInfo::default()
            .image_view(glyph)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .sampler(sampler)];
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(image)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .sampler(sampler)];
        let color_glyph_info = [vk::DescriptorImageInfo::default()
            .image_view(color_glyph)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .sampler(sampler)];
        let mut writes = Vec::with_capacity(3 * MAX_WINDOWS as usize * MAX_FRAMES_IN_FLIGHT);
        for set in sets {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(BINDING_GLYPH_ATLAS)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&glyph_info),
            );
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(BINDING_IMAGE_ATLAS)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&image_info),
            );
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(BINDING_COLOR_GLYPH_ATLAS)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&color_glyph_info),
            );
        }
        if writes.is_empty() {
            return;
        }
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_pipeline(self.quad, None);
            for set in [self.geometry.list, self.geometry.strip] {
                self.device.destroy_pipeline(set.ui, None);
                self.device.destroy_pipeline(set.image, None);
                self.device.destroy_pipeline(set.glyph, None);
            }
            self.device.destroy_render_pass(self.pass_clear, None);
            self.device.destroy_render_pass(self.pass_load, None);
            self.device.destroy_pipeline_layout(self.layout, None);
            self.device
                .destroy_descriptor_set_layout(self.set_layout, None);
        }
        self.cache.write_out();
    }
}

struct PipelineCache {
    device: ash::Device,
    raw: vk::PipelineCache,
    path: Option<PathBuf>,
    header: Vec<u8>,
}

const CACHE_MAGIC: [u8; 4] = *b"KYTP";
const CACHE_VERSION: u32 = 1;

impl PipelineCache {
    fn new(device: &Device) -> Self {
        let properties = device.properties();
        let uuid = properties.pipeline_cache_uuid;
        let header = cache_header(&uuid, properties.driver_version, &device_name(properties));
        let path = cache_file_path();
        let initial = path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .filter(|bytes| bytes.starts_with(&header))
            .map(|bytes| cache_payload(&bytes, header.len()).to_vec());
        let create = vk::PipelineCacheCreateInfo::default();
        let create = match initial.as_ref() {
            Some(blob) => create.initial_data(blob),
            None => create,
        };
        let raw = unsafe { device.raw().create_pipeline_cache(&create, None) }
            .unwrap_or(vk::PipelineCache::null());
        Self {
            device: device.raw().clone(),
            raw,
            path,
            header,
        }
    }

    fn raw(&self) -> vk::PipelineCache {
        self.raw
    }

    fn write_out(&self) {
        if self.raw == vk::PipelineCache::null() {
            return;
        }
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let Ok(data) = (unsafe { self.device.get_pipeline_cache_data(self.raw) }) else {
            return;
        };
        if data.is_empty() {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut bytes = Vec::with_capacity(self.header.len() + data.len());
        bytes.extend_from_slice(&self.header);
        bytes.extend_from_slice(&data);
        let temporary =
            path.with_extension(smol_str::format_smolstr!("{}.tmp", std::process::id()));
        let _ = std::fs::write(&temporary, &bytes).and_then(|()| std::fs::rename(&temporary, path));
    }
}

impl Drop for PipelineCache {
    fn drop(&mut self) {
        if self.raw != vk::PipelineCache::null() {
            unsafe { self.device.destroy_pipeline_cache(self.raw, None) };
        }
    }
}

fn device_name(properties: &vk::PhysicalDeviceProperties) -> String {
    unsafe { std::ffi::CStr::from_ptr(properties.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn cache_file_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("kytrix").join("pipeline-cache.bin"))
}

fn cache_header(uuid: &[u8; 16], driver_version: u32, device_name: &str) -> Vec<u8> {
    let name = device_name.as_bytes();
    let name = &name[..name.len().min(u8::MAX as usize)];
    let mut header = Vec::with_capacity(4 + 4 + 16 + 4 + 1 + name.len());
    header.extend_from_slice(&CACHE_MAGIC);
    header.extend_from_slice(&CACHE_VERSION.to_le_bytes());
    header.extend_from_slice(uuid);
    header.extend_from_slice(&driver_version.to_le_bytes());
    header.push(name.len() as u8);
    header.extend_from_slice(name);
    header
}

fn cache_payload(bytes: &[u8], header_len: usize) -> &[u8] {
    bytes.get(header_len..).unwrap_or(&[])
}

pub(crate) struct DescriptorPool {
    device: ash::Device,
    pool: vk::DescriptorPool,
    chunks: Vec<[vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT]>,
    free: Vec<usize>,
}

impl DescriptorPool {
    pub(crate) fn new(
        device: &ash::Device,
        set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, EngineError> {
        let windows = MAX_WINDOWS as usize;
        let set_count = (windows * MAX_FRAMES_IN_FLIGHT) as u32;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(set_count * 2),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(set_count * 2),
        ];
        let pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(set_count)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .map_err(|error| init_error("create_descriptor_pool", format!("{error:?}")))?;
        let layouts = vec![set_layout; set_count as usize];
        let sets = match unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
        } {
            Ok(sets) => sets,
            Err(error) => {
                unsafe { device.destroy_descriptor_pool(pool, None) };
                return Err(init_error("allocate_descriptor_sets", format!("{error:?}")));
            }
        };
        let mut chunks = Vec::with_capacity(windows);
        for chunk in sets.chunks(MAX_FRAMES_IN_FLIGHT) {
            let mut slot = [vk::DescriptorSet::null(); MAX_FRAMES_IN_FLIGHT];
            slot.copy_from_slice(chunk);
            chunks.push(slot);
        }
        Ok(Self {
            device: device.clone(),
            pool,
            chunks,
            free: (0..windows).rev().collect(),
        })
    }

    pub(crate) fn allocate(&mut self) -> Option<usize> {
        self.free.pop()
    }

    pub(crate) fn free(&mut self, chunk: usize) {
        debug_assert!(
            chunk < self.chunks.len(),
            "descriptor chunk index must be in range"
        );
        self.free.push(chunk);
    }

    pub(crate) fn chunk(&self, chunk: usize) -> [vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT] {
        self.chunks[chunk]
    }

    pub(crate) fn all_sets(&self) -> impl Iterator<Item = vk::DescriptorSet> + '_ {
        self.chunks.iter().flatten().copied()
    }
}

impl Drop for DescriptorPool {
    fn drop(&mut self) {
        unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
    }
}

fn create_render_pass(
    device: &ash::Device,
    load_op: vk::AttachmentLoadOp,
    initial_layout: vk::ImageLayout,
) -> Result<vk::RenderPass, EngineError> {
    let attachments = [vk::AttachmentDescription::default()
        .format(RENDER_FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(load_op)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(initial_layout)
        .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
    let references = [vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
    let subpasses = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&references)];
    let dependencies = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
        .dst_access_mask(
            vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        )
        .dependency_flags(vk::DependencyFlags::BY_REGION)];
    unsafe {
        device.create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(&attachments)
                .subpasses(&subpasses)
                .dependencies(&dependencies),
            None,
        )
    }
    .map_err(|error| init_error("create_render_pass", format!("{error:?}")))
}

fn create_quad_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
    pass: vk::RenderPass,
) -> Result<vk::Pipeline, EngineError> {
    let vs = shader_module(
        device,
        include_bytes!(concat!(env!("OUT_DIR"), "/shaders/quad.vs.spv")),
        QUAD_VS,
    )?;
    let ps = shader_module(
        device,
        include_bytes!(concat!(env!("OUT_DIR"), "/shaders/quad.ps.spv")),
        QUAD_PS,
    )?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vs)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(ps)
            .name(c"main"),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA);
    let attachments = [blend_attachment];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let create = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .render_pass(pass);
    let pipelines = unsafe { device.create_graphics_pipelines(cache, &[create], None) };
    unsafe {
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(ps, None);
    }
    pipelines
        .map_err(|(_, error)| init_error("create_graphics_pipelines", format!("{error:?}")))
        .map(|pipelines| pipelines[0])
}

fn create_geometry_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
    pass: vk::RenderPass,
    vertex_shader: &str,
    fragment_shader: &str,
    stride: u32,
    topology: vk::PrimitiveTopology,
) -> Result<vk::Pipeline, EngineError> {
    let vertex_path = format!("{}/shaders/{vertex_shader}", env!("OUT_DIR"));
    let fragment_path = format!("{}/shaders/{fragment_shader}", env!("OUT_DIR"));
    let (vertex_bytes, fragment_bytes) = match (vertex_shader, fragment_shader) {
        ("geometry.vs_ui.spv", "geometry.ps_ui.spv") => (
            include_bytes!(concat!(env!("OUT_DIR"), "/shaders/geometry.vs_ui.spv")).as_slice(),
            include_bytes!(concat!(env!("OUT_DIR"), "/shaders/geometry.ps_ui.spv")).as_slice(),
        ),
        ("geometry.vs_textured.spv", "geometry.ps_textured_image.spv") => (
            include_bytes!(concat!(
                env!("OUT_DIR"),
                "/shaders/geometry.vs_textured.spv"
            ))
            .as_slice(),
            include_bytes!(concat!(
                env!("OUT_DIR"),
                "/shaders/geometry.ps_textured_image.spv"
            ))
            .as_slice(),
        ),
        _ => (
            include_bytes!(concat!(
                env!("OUT_DIR"),
                "/shaders/geometry.vs_textured.spv"
            ))
            .as_slice(),
            include_bytes!(concat!(
                env!("OUT_DIR"),
                "/shaders/geometry.ps_textured_glyph.spv"
            ))
            .as_slice(),
        ),
    };
    let vs = shader_module(device, vertex_bytes, &vertex_path)?;
    let ps = shader_module(device, fragment_bytes, &fragment_path)?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vs)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(ps)
            .name(c"main"),
    ];
    let position = vk::VertexInputAttributeDescription::default()
        .location(0)
        .binding(0)
        .format(vk::Format::R32G32_SFLOAT)
        .offset(0);
    let texcoord = vk::VertexInputAttributeDescription::default()
        .location(1)
        .binding(0)
        .format(vk::Format::R32G32_SFLOAT)
        .offset(8);
    let coverage = vk::VertexInputAttributeDescription::default()
        .location(1)
        .binding(0)
        .format(vk::Format::R32_SFLOAT)
        .offset(8);
    let attributes: Vec<vk::VertexInputAttributeDescription> = match stride {
        16 => vec![position, texcoord],
        12 => vec![position, coverage],
        _ => vec![position],
    };
    let bindings = [vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(stride)
        .input_rate(vk::VertexInputRate::VERTEX)];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(topology);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA);
    let attachments = [blend_attachment];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let create = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .render_pass(pass);
    let pipelines = unsafe { device.create_graphics_pipelines(cache, &[create], None) };
    unsafe {
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(ps, None);
    }
    pipelines
        .map_err(|(_, error)| init_error("create_graphics_pipelines", format!("{error:?}")))
        .map(|pipelines| pipelines[0])
}

fn shader_module(
    device: &ash::Device,
    bytes: &[u8],
    path: &str,
) -> Result<vk::ShaderModule, EngineError> {
    let shader_error = EngineError::ShaderLoad {
        target: "spirv",
        path: PathBuf::from(path),
    };
    if bytes.starts_with(SKIPPED) || !bytes.starts_with(&SPIRV_MAGIC) {
        return Err(shader_error);
    }
    let (words, _) = bytes.as_chunks::<4>();
    let words: Vec<u32> = words.iter().map(|word| u32::from_le_bytes(*word)).collect();
    unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
    }
    .map_err(|_| shader_error)
}

#[cfg(test)]
mod tests {
    use super::{
        CACHE_MAGIC, CACHE_VERSION, DescriptorPool, MAX_FRAMES_IN_FLIGHT, MAX_WINDOWS,
        cache_header, cache_payload,
    };
    use crate::render::linux::device::{SharedGpu, gpu_test_lock};

    #[test]
    fn the_pipeline_cache_header_identifies_the_device() {
        let uuid = [7u8; 16];
        let header = cache_header(&uuid, 0x1234_5678, "llvmpipe (LLVM 20.1.0)");
        assert_eq!(&header[..4], &CACHE_MAGIC);
        assert_eq!(
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
            CACHE_VERSION
        );
        assert_eq!(&header[8..24], &uuid);
        assert_eq!(
            u32::from_le_bytes([header[24], header[25], header[26], header[27]]),
            0x1234_5678
        );
        let name_len = header[28] as usize;
        assert_eq!(&header[29..29 + name_len], b"llvmpipe (LLVM 20.1.0)");
        assert_eq!(header.len(), 29 + name_len);

        let other_uuid = [8u8; 16];
        let other = cache_header(&other_uuid, 0x1234_5678, "llvmpipe (LLVM 20.1.0)");
        assert!(!header.starts_with(&other));
        let other = cache_header(&uuid, 0x9999_9999, "llvmpipe (LLVM 20.1.0)");
        assert!(!header.starts_with(&other));
        let other = cache_header(&uuid, 0x1234_5678, "NVIDIA GeForce RTX 4090");
        assert!(!header.starts_with(&other));
    }

    #[test]
    fn an_overlong_device_name_is_truncated() {
        let long = "x".repeat(4096);
        let header = cache_header(&[0u8; 16], 1, &long);
        assert_eq!(header[28], u8::MAX);
        assert_eq!(header.len(), 29 + u8::MAX as usize);
    }

    #[test]
    fn the_pipeline_cache_payload_skips_the_custom_header() {
        let header = cache_header(&[1u8; 16], 7, "dev");
        let mut file = header.clone();
        file.extend_from_slice(b"driver-blob");
        assert_eq!(
            cache_payload(&file, header.len()),
            b"driver-blob",
            "payload 必须从头部之后开始"
        );
        assert!(cache_payload(&header, header.len()).is_empty());
        assert!(cache_payload(&[], 40).is_empty());
    }

    #[test]
    fn the_descriptor_pool_hands_out_exactly_max_windows_segments() {
        let _guard = gpu_test_lock();
        let shared = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let mut pool = DescriptorPool::new(shared.raw(), shared.pipeline().set_layout())
            .expect("descriptor pool");
        let mut chunks = Vec::with_capacity(MAX_WINDOWS as usize);
        while let Some(chunk) = pool.allocate() {
            assert!(chunk < MAX_WINDOWS as usize);
            chunks.push(chunk);
            assert!(
                chunks.len() <= MAX_WINDOWS as usize,
                "池不得给出超过 MAX_WINDOWS 段"
            );
        }
        assert_eq!(chunks.len(), MAX_WINDOWS as usize, "每个窗口一段");
        assert!(pool.allocate().is_none(), "用尽之后必须明确失败");

        assert_eq!(pool.chunk(chunks[0]).len(), MAX_FRAMES_IN_FLIGHT);
        pool.free(chunks[0]);
        assert_eq!(pool.allocate(), Some(chunks[0]));
        assert_eq!(
            pool.all_sets().count(),
            MAX_WINDOWS as usize * MAX_FRAMES_IN_FLIGHT
        );
    }
}

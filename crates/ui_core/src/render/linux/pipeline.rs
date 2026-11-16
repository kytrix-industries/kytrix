//! 固定 descriptor set + push constants + uber-quad / geometry 管线
//! （`design/phase-12-macos-linux.md` §6.4）。
//!
//! 与 P07 的 `common.glsl` 逐字对应：`set 0` 的 `binding 0`（instances）/`1`（clips）/
//! `4` + `5`（两张图集 sampler），push constants `[0, 112)`。
//!
//! geometry 管线按 `GeometryPipeline` 分成三种（`Ui` / `Textured{Image}` / `Textured{Glyph}`），
//! 顶点布局由 pipeline 唯一决定（`contract.md` §7）：`Ui` = 8 B（`float2` 位置）、
//! `Textured` = 16 B（`float2` 位置 + `float2` uv）。

use std::path::PathBuf;

use ash::vk;

use crate::{
    error::EngineError,
    paint::{atlas::AtlasKind, geometry::GeometryPipeline},
    render::{
        frame::MAX_FRAMES_IN_FLIGHT,
        linux::device::{Device, init_error},
    },
};

/// `RootUniform` 的字节数（P07 §2.7）。
pub(crate) const ROOT_UNIFORM_BYTES: u32 = 112;

/// 编译期产物（`build.rs` 写进 `OUT_DIR/shaders`）。
const QUAD_VS: &str = concat!(env!("OUT_DIR"), "/shaders/quad.vs.spv");
const QUAD_PS: &str = concat!(env!("OUT_DIR"), "/shaders/quad.ps.spv");
/// SPIR-V 的魔数（`build.rs` 已校验，这里再查一次）。
const SPIRV_MAGIC: [u8; 4] = [0x03, 0x02, 0x23, 0x07];
/// 占位产物前缀。
const SKIPPED: &[u8] = b"KYTRIX-SHADER-SKIPPED";

/// 三种几何管线的 PSO（按 `GeometryPipeline` 与图集种类分派）。
#[derive(Clone, Copy)]
pub(crate) struct GeometryPipelines {
    ui: vk::Pipeline,
    image: vk::Pipeline,
    glyph: vk::Pipeline,
}

impl GeometryPipelines {
    /// 按注册描述选 PSO。
    pub(crate) fn get(self, pipeline: GeometryPipeline) -> vk::Pipeline {
        match pipeline {
            GeometryPipeline::Ui => self.ui,
            GeometryPipeline::Textured {
                atlas: AtlasKind::Image,
            } => self.image,
            GeometryPipeline::Textured {
                atlas: AtlasKind::Glyph,
            } => self.glyph,
        }
    }
}

/// 渲染通道 / 描述符 / 管线。
pub(crate) struct Pipeline {
    device: ash::Device,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    layout: vk::PipelineLayout,
    pass_clear: vk::RenderPass,
    pass_load: vk::RenderPass,
    quad: vk::Pipeline,
    geometry: GeometryPipelines,
}

impl Pipeline {
    pub(crate) fn new(device: &Device, format: vk::Format) -> Result<Self, EngineError> {
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                // `quad.frag.glsl` 也读 instances（b0），只给 VERTEX 会是未定义行为。
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(5)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        // SAFETY: 描述在本次调用期间有效；句柄在 Drop 里销毁。
        let set_layout = unsafe {
            device.raw().create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .map_err(|error| init_error("create_descriptor_set_layout", format!("{error:?}")))?;

        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(2 * MAX_FRAMES_IN_FLIGHT as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(2 * MAX_FRAMES_IN_FLIGHT as u32),
        ];
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let pool = unsafe {
            device.raw().create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .map_err(|error| init_error("create_descriptor_pool", format!("{error:?}")))?;
        let layouts = vec![set_layout; MAX_FRAMES_IN_FLIGHT];
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let sets = unsafe {
            device.raw().allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
        }
        .map_err(|error| init_error("allocate_descriptor_sets", format!("{error:?}")))?;

        let set_layouts = [set_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(ROOT_UNIFORM_BYTES)];
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let layout = unsafe {
            device.raw().create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push),
                None,
            )
        }
        .map_err(|error| init_error("create_pipeline_layout", format!("{error:?}")))?;

        // CLEAR 通道从 `UNDEFINED` 起（persist 没有预先 transition）；LOAD 通道承接上一帧的布局。
        let pass_clear = create_render_pass(
            device,
            format,
            vk::AttachmentLoadOp::CLEAR,
            vk::ImageLayout::UNDEFINED,
        )?;
        let pass_load = create_render_pass(
            device,
            format,
            vk::AttachmentLoadOp::LOAD,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        )?;
        let quad = create_quad_pipeline(device, layout, pass_clear)?;
        let geometry = GeometryPipelines {
            ui: create_geometry_pipeline(
                device,
                layout,
                pass_clear,
                "geometry.vs_ui.spv",
                "geometry.ps_ui.spv",
                8,
                vk::PrimitiveTopology::TRIANGLE_LIST,
            )?,
            image: create_geometry_pipeline(
                device,
                layout,
                pass_clear,
                "geometry.vs_textured.spv",
                "geometry.ps_textured_image.spv",
                16,
                vk::PrimitiveTopology::TRIANGLE_LIST,
            )?,
            glyph: create_geometry_pipeline(
                device,
                layout,
                pass_clear,
                "geometry.vs_textured.spv",
                "geometry.ps_textured_glyph.spv",
                16,
                vk::PrimitiveTopology::TRIANGLE_LIST,
            )?,
        };

        Ok(Self {
            device: device.raw().clone(),
            set_layout,
            pool,
            sets,
            layout,
            pass_clear,
            pass_load,
            quad,
            geometry,
        })
    }

    /// 按注册描述选 geometry PSO。
    pub(crate) fn geometry(&self, pipeline: GeometryPipeline) -> vk::Pipeline {
        self.geometry.get(pipeline)
    }

    /// 随 `clear` 选择两个渲染通道之一（`loadOp` 是渲染通道的属性，不能逐帧改）。
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

    /// 把某槽的实例/裁剪缓冲写进 descriptor set（单个上传缓冲，clips 在 `clip_offset` 处）。
    ///
    /// 两段都用 `VK_WHOLE_SIZE`：空实例集时 `range = 0` 是**非法**描述符，而 `WHOLE_SIZE`
    /// 从 offset 到缓冲末尾，天然覆盖「clips 在前、instances 可能为空」的所有情形。
    /// `clip_offset` 必须按 `minStorageBufferOffsetAlignment`（最多 256 B）向上对齐。
    pub(crate) fn write_slot(&self, slot: usize, upload: vk::Buffer, clip_offset: u64) {
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
                .dst_set(self.sets[slot])
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&instance_info),
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[slot])
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&clip_info),
        ];
        // SAFETY: 写入的 buffer 由 `Renderer` 持有且存活期覆盖 descriptor set。
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
    }

    /// 两张图集的视图 + 采样器写进所有槽（图集重建后重写）。
    pub(crate) fn write_atlas(
        &self,
        glyph: vk::ImageView,
        image: vk::ImageView,
        sampler: vk::Sampler,
    ) {
        for set in &self.sets {
            let glyph_info = [vk::DescriptorImageInfo::default()
                .image_view(glyph)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .sampler(sampler)];
            let image_info = [vk::DescriptorImageInfo::default()
                .image_view(image)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .sampler(sampler)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(4)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&glyph_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(5)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&image_info),
            ];
            // SAFETY: 视图/采样器由 `AtlasTextures` 持有且存活期覆盖 descriptor set。
            unsafe { self.device.update_descriptor_sets(&writes, &[]) };
        }
    }

    /// 某槽的 descriptor set（`cmd_bind_descriptor_sets` 用）。
    pub(crate) fn set(&self, slot: usize) -> vk::DescriptorSet {
        self.sets[slot]
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // SAFETY: 句柄归本结构独占；`Renderer::drop` 先 `wait_idle`。
        unsafe {
            self.device.destroy_pipeline(self.quad, None);
            self.device.destroy_pipeline(self.geometry.ui, None);
            self.device.destroy_pipeline(self.geometry.image, None);
            self.device.destroy_pipeline(self.geometry.glyph, None);
            self.device.destroy_render_pass(self.pass_clear, None);
            self.device.destroy_render_pass(self.pass_load, None);
            self.device.destroy_pipeline_layout(self.layout, None);
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device
                .destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

fn create_render_pass(
    device: &Device,
    format: vk::Format,
    load_op: vk::AttachmentLoadOp,
    initial_layout: vk::ImageLayout,
) -> Result<vk::RenderPass, EngineError> {
    let attachments = [vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(load_op)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        // persist 的 layout 由 `Renderer` 显式管理（进 = 出 = COLOR_ATTACHMENT_OPTIMAL）。
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
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
    // SAFETY: 描述在本次调用期间有效。
    unsafe {
        device.raw().create_render_pass(
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
    device: &Device,
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
        // P07 §2.9：三平台统一 cull = NONE。
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
    // SAFETY: `create` 的指针在本次调用期间有效；pipeline 在 Drop 里销毁。
    let pipelines = unsafe {
        device
            .raw()
            .create_graphics_pipelines(vk::PipelineCache::null(), &[create], None)
    };
    // shader module 建完管线即可销毁（管线已持有其副本）。
    // SAFETY: 两个 module 归本函数独占且已不被引用。
    unsafe {
        device.raw().destroy_shader_module(vs, None);
        device.raw().destroy_shader_module(ps, None);
    }
    pipelines
        .map_err(|(_, error)| init_error("create_graphics_pipelines", format!("{error:?}")))
        .map(|pipelines| pipelines[0])
}

/// 建一条 geometry PSO。
///
/// 与 uber-quad 的差别只有顶点输入（`stride` 与属性表）与拓扑：其余状态（cull = NONE、
/// 无深度/模板、MSAA 关闭、`ONE / INV_SRC_ALPHA` 混合）逐字相同（`contract.md` §7）。
///
/// `stride` = 8 时只有 `POSITION`（`Ui`）；16 时再加 `TEXCOORD`（`Textured`）。
fn create_geometry_pipeline(
    device: &Device,
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
    let attributes: Vec<vk::VertexInputAttributeDescription> = if stride == 16 {
        vec![position, texcoord]
    } else {
        vec![position]
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
    // SAFETY: `create` 的指针在本次调用期间有效；pipeline 在 Drop 里销毁。
    let pipelines = unsafe {
        device
            .raw()
            .create_graphics_pipelines(vk::PipelineCache::null(), &[create], None)
    };
    // SAFETY: 两个 module 归本函数独占且已不被引用。
    unsafe {
        device.raw().destroy_shader_module(vs, None);
        device.raw().destroy_shader_module(ps, None);
    }
    pipelines
        .map_err(|(_, error)| init_error("create_graphics_pipelines", format!("{error:?}")))
        .map(|pipelines| pipelines[0])
}

fn shader_module(
    device: &Device,
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
    // SAFETY: `words` 是对齐后的本地 `Vec<u32>`，长度 = 代码字数；调用期间有效。
    unsafe {
        device
            .raw()
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
    }
    .map_err(|_| shader_error)
}

//! 固定 descriptor set + push constants + uber-quad / geometry 管线（`design/phase-12-macos-linux.md`
//! §6.4）+ 进程级描述符池（P1-2.9）。
//!
//! 与 P07 的 `common.glsl` 逐字对应：`set 0` 的 `binding 0`（instances）/`1`（clips）/
//! `4` + `5`（两张图集 sampler），push constants `[0, 112)`。
//!
//! 多窗口分层（与 Windows 后端的 `Pipeline` / `SrvHeap` + `DescriptorLease` 同构）：
//!
//! - [`Pipeline`]（`render pass` / `PipelineLayout` / `VkPipeline` 集合）是**进程级**资源，由
//!   `device::SharedGpu` 持有、创建后只读；固定使用 [`RENDER_FORMAT`]（persist 与 swapchain
//!   同格式，整面 copy 才合法）。
//! - [`DescriptorPool`] 是进程级的一只 `VkDescriptorPool`：启动时按 [`MAX_WINDOWS`] 预分配好
//!   「每窗口 `MAX_FRAMES_IN_FLIGHT` 个 set」的固定段，窗口创建时拿走一段、销毁时归还。
//!   `vkUpdateDescriptorSets` 要求 descriptor pool 外部同步，因此所有写描述符的调用都由
//!   `SharedGpu` 的池锁串行化。
//!
//! geometry 管线按 `GeometryPipeline` 分成三种（`Ui` / `Textured{Image}` / `Textured{Glyph}`），
//! 顶点布局由 pipeline 唯一决定（`contract.md` §7）：`Ui` = **12 B**（`float2` 位置 + `f32`
//! 覆盖率距离）、`Textured` = 16 B（`float2` 位置 + `float2` uv）。PSO 还必须按
//! `GeometryDesc.topology` 各建一条（Vulkan 的拓扑是 PSO 状态，不能逐 draw 切换）。

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

/// `RootUniform` 的字节数（P07 §2.7）。
pub(crate) const ROOT_UNIFORM_BYTES: u32 = 112;

/// 进程级渲染目标格式：persist、render pass 与 swapchain image 必须一致（`vkCmdCopyImage`
/// 要求两侧格式**兼容**；B8G8R8A8_SRGB 与 R8G8B8A8_SRGB 不是兼容格式）。Wayland surface
/// 不提供该格式时按 D8 硬基线报 `UnsupportedGpu`，不做 R8 回退。
pub(crate) const RENDER_FORMAT: vk::Format = vk::Format::B8G8R8A8_SRGB;

/// 共享描述符池支持的窗口段数（每段 = `MAX_FRAMES_IN_FLIGHT` 个 set）。
///
/// 64 个窗口远超桌面应用的常规需要；超出时窗口创建给出明确错误（而不是让描述符池悄悄
/// 分配失败）。
pub(crate) const MAX_WINDOWS: u32 = 64;

/// `set 0` 的两个 SSBO 描述符（instances / clips）与两个图集 sampler 的绑定号。
const BINDING_INSTANCES: u32 = 0;
const BINDING_CLIPS: u32 = 1;
const BINDING_GLYPH_ATLAS: u32 = 4;
const BINDING_IMAGE_ATLAS: u32 = 5;

/// 编译期产物（`build.rs` 写进 `OUT_DIR/shaders`）。
const QUAD_VS: &str = concat!(env!("OUT_DIR"), "/shaders/quad.vs.spv");
const QUAD_PS: &str = concat!(env!("OUT_DIR"), "/shaders/quad.ps.spv");
/// SPIR-V 的魔数（`build.rs` 已校验，这里再查一次）。
const SPIRV_MAGIC: [u8; 4] = [0x03, 0x02, 0x23, 0x07];
/// 占位产物前缀。
const SKIPPED: &[u8] = b"KYTRIX-SHADER-SKIPPED";

/// 一套 geometry PSO：三种 pipeline 变体在**同一种拓扑**下的集合。
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
                atlas: AtlasKind::Glyph,
            } => self.glyph,
        }
    }
}

/// 全部 geometry PSO：**每个 pipeline 变体 × 每种拓扑**各一条（共 6 条）。
///
/// Vulkan 的 `VkPipelineInputAssemblyStateCreateInfo::topology` 是 PSO 的烘死状态，不能像 D3D12
/// 的 `IASetPrimitiveTopology` 那样逐 draw 切换（Metal 也按 `desc.topology` 建两条 PSO）。旧实现
/// 把拓扑烘成恒 `TRIANGLE_LIST`，于是 `Topology::TriangleStrip` 的几何在 Linux 上被按 list 画
/// （AR2 P3-② 的跨端几何正确性差异）。
#[derive(Clone, Copy)]
pub(crate) struct GeometryPipelines {
    list: GeometryPipelineSet,
    strip: GeometryPipelineSet,
}

impl GeometryPipelines {
    /// 按注册描述选 PSO（`GeometryDesc.topology` 参与选择，与 D3D12 / Metal 同语义）。
    pub(crate) fn get(self, pipeline: GeometryPipeline, topology: Topology) -> vk::Pipeline {
        match topology {
            Topology::TriangleList => self.list.get(pipeline),
            Topology::TriangleStrip => self.strip.get(pipeline),
        }
    }
}

/// 进程级的渲染通道 / 管线布局 / PSO 集合（创建后只读，多窗口共享）。
pub(crate) struct Pipeline {
    device: ash::Device,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pass_clear: vk::RenderPass,
    pass_load: vk::RenderPass,
    quad: vk::Pipeline,
    geometry: GeometryPipelines,
    /// 进程级 `VkPipelineCache`（R11-14）：启动时从缓存文件恢复，退出时写回。
    ///
    /// **声明在最后**：字段按声明顺序 drop，缓存要在全部 PSO 销毁之后才写回 / 销毁。
    cache: PipelineCache,
}

impl Pipeline {
    /// 建 `set 0` 布局、管线布局、两条 render pass（clear / load）与全部 PSO。
    ///
    /// PSO 走进程级 `VkPipelineCache`（落盘，见 [`PipelineCache`]）：旧实现每次启动都用
    /// `vk::PipelineCache::null()` 同步编译 4 条 PSO（`create_quad_pipeline` + 6 条 geometry），
    /// Mesa（radv/anv）与 NVIDIA 上每条几十到几百毫秒，lavapipe 更慢 —— 窗口开出后空白几百
    /// 毫秒，且每次启动重复付费（R11-14）。
    pub(crate) fn new(device: &Device) -> Result<Self, EngineError> {
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(BINDING_INSTANCES)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                // `quad.frag.glsl` 也读 instances（b0），只给 VERTEX 会是未定义行为。
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
        ];
        // SAFETY: 描述在本次调用期间有效；句柄在 Drop 里销毁。
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

        // R11-14：进程级 PSO 缓存（落盘）。建在这里而不是 SharedGpu 里，是因为它的生命周期
        // 正好等于「全部 PSO + render pass 的集合」：drop 时先销毁 PSO，再写回缓存数据。
        let cache = PipelineCache::new(device);

        // CLEAR 通道从 `UNDEFINED` 起（persist 没有预先 transition）；LOAD 通道承接上一帧的布局。
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

    /// `set 0` 的布局（`DescriptorPool` 按它分配各窗口的 set）。
    pub(crate) fn set_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }

    /// 按注册描述选 geometry PSO（拓扑也参与选择：Vulkan 的拓扑是 PSO 烘死状态）。
    pub(crate) fn geometry(&self, pipeline: GeometryPipeline, topology: Topology) -> vk::Pipeline {
        self.geometry.get(pipeline, topology)
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

    /// 把某帧槽的实例/裁剪缓冲写进该窗口的 descriptor set（单个上传缓冲，clips 在
    /// `clip_offset` 处）。
    ///
    /// 两段都用 `VK_WHOLE_SIZE`：空实例集时 `range = 0` 是**非法**描述符，而 `WHOLE_SIZE`
    /// 从 offset 到缓冲末尾，天然覆盖「clips 在前、instances 可能为空」的所有情形。
    /// `clip_offset` 必须按 `minStorageBufferOffsetAlignment`（最多 256 B）向上对齐。
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
        // SAFETY: 写入的 buffer 由窗口的上传缓冲持有且存活期覆盖 descriptor set。
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
    }

    /// 把两张图集视图 + 采样器写进**所有**窗口的 descriptor set（图集换代后重写）。
    ///
    /// 图集页是进程级共享资源，而图集绑定在每个窗口自己的 set 里，因此换代必须广播。
    /// 调用方（`SharedGpu::rebuild_atlas_pages`）持图集写锁并已等队列 idle，且持描述符池锁：
    /// 此刻没有在飞命令引用旧视图，也没有别的线程在写描述符。
    pub(crate) fn write_atlas(
        &self,
        sets: impl Iterator<Item = vk::DescriptorSet>,
        glyph: vk::ImageView,
        image: vk::ImageView,
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
        let mut writes = Vec::with_capacity(2 * MAX_WINDOWS as usize * MAX_FRAMES_IN_FLIGHT);
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
        }
        if writes.is_empty() {
            return;
        }
        // SAFETY: 视图 / 采样器由共享图集持有；descriptor set 都在本进程的描述符池里。
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // SAFETY: 句柄归本结构独占；共享设备由 `SharedGpu` 保活到本结构 drop 之后。
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
        // 全部 PSO 都已销毁：这时写回的缓存才是完整的一份（R11-14）。
        self.cache.write_out();
    }
}

/// `VkPipelineCache` 的落盘缓存文件（R11-14）。
///
/// 文件位置：`$XDG_CACHE_HOME/kytrix/pipeline-cache.bin`（没有 `XDG_CACHE_HOME` 时退回
/// `$HOME/.cache/...`；两者都没有就不落盘，只在进程内复用）。
///
/// 头部（自定义，紧跟 Vulkan 的 blob 之前）记录 `pipelineCacheUUID` + `driverVersion` +
/// `deviceName`：驱动升级 / 换卡之后旧的 blob 必须丢弃 —— 用错缓存轻则编译被跳过（错误的
/// 机器码），重则驱动崩溃。`VkPipelineCache` 的 blob 本身由驱动校验，但我们无法在
/// `vkCreatePipelineCache` 失败时避免泄漏，因此**先自己校验头部**再用。
struct PipelineCache {
    device: ash::Device,
    raw: vk::PipelineCache,
    /// `None` = 没有可写的缓存目录（容器 / 无家目录）。
    path: Option<PathBuf>,
    /// 写入文件的自定义头部（设备身份；读盘时用它做前缀比较）。
    header: Vec<u8>,
}

/// 缓存文件的魔数（`KYTP` = Kytrix pipeline）。
const CACHE_MAGIC: [u8; 4] = *b"KYTP";
/// 头部版本：布局变化时必须前进（旧文件因此被丢弃）。
const CACHE_VERSION: u32 = 1;

impl PipelineCache {
    /// 读缓存文件（头部不匹配就丢弃）→ 建 `VkPipelineCache`。
    ///
    /// 任何一步失败都退回「没有缓存」：PSO 仍能建出来，只是没有冷启动加速
    /// （**不做**「缓存坏了就报错」的降级 —— 那会把可选的性能优化变成启动失败）。
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
        // SAFETY: `create` 的指针在本次调用期间有效；句柄归本结构独占。
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

    /// 把当前缓存写回磁盘（`get_pipeline_cache_data` 允许返回比实际更短的 blob）。
    fn write_out(&self) {
        if self.raw == vk::PipelineCache::null() {
            return;
        }
        let Some(path) = self.path.as_ref() else {
            return;
        };
        // SAFETY: 缓存句柄归本结构独占；本进程是唯一使用者（没有并发写）。
        let Ok(data) = (unsafe { self.device.get_pipeline_cache_data(self.raw) }) else {
            return;
        };
        // 空 blob 不值得写盘（驱动不支持序列化时会返回 0 字节）。
        if data.is_empty() {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut bytes = Vec::with_capacity(self.header.len() + data.len());
        bytes.extend_from_slice(&self.header);
        bytes.extend_from_slice(&data);
        // **原子落盘**：先写同目录下的临时文件再 rename。直接 `write` 在断电 / 磁盘满时可能留下
        // 「头部完整、blob 截断」的文件 —— 下次启动头部会通过校验，`vkCreatePipelineCache` 收到
        // 坏 blob 返回 `ERROR_INVALID_PIPELINE_CACHE_DATA`，而 ash 的 `assume_init_on_success`
        // 会连**有效句柄**一起丢掉（规范要求这种情况下仍返回可用句柄）⇒ 泄漏一个 cache 句柄，
        // 且本次运行完全没有 PSO 缓存。rename 之后文件要么是旧的要么是新的，不会截断。
        let temporary =
            path.with_extension(smol_str::format_smolstr!("{}.tmp", std::process::id()));
        let _ = std::fs::write(&temporary, &bytes).and_then(|()| std::fs::rename(&temporary, path));
    }
}

impl Drop for PipelineCache {
    fn drop(&mut self) {
        // 写回由 Pipeline::drop 负责（在全部 PSO 销毁之后）：这里只销毁句柄。
        if self.raw != vk::PipelineCache::null() {
            // SAFETY: 句柄归本结构独占；PSO 在 `Pipeline::drop` 里已先销毁。
            unsafe { self.device.destroy_pipeline_cache(self.raw, None) };
        }
    }
}

/// `VkPhysicalDeviceProperties::deviceName`（NUL 结尾）。
fn device_name(properties: &vk::PhysicalDeviceProperties) -> String {
    // SAFETY: `device_name` 是 Vulkan 保证的 NUL 结尾字符串。
    unsafe { std::ffi::CStr::from_ptr(properties.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// 缓存文件路径（`$XDG_CACHE_HOME/kytrix/pipeline-cache.bin`，退回 `$HOME/.cache/...`）。
fn cache_file_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("kytrix").join("pipeline-cache.bin"))
}

/// 头部字节：`magic | version | uuid(16) | driverVersion | nameLen(u8) | name`。
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

/// 去掉自定义头部之后的驱动 blob（越界返回空）。
fn cache_payload(bytes: &[u8], header_len: usize) -> &[u8] {
    bytes.get(header_len..).unwrap_or(&[])
}

/// 进程级描述符池：按 `MAX_WINDOWS` 段预分配「每窗口 `MAX_FRAMES_IN_FLIGHT` 个 set」。
///
/// 段的下标即窗口租约的标识（[`super::device::DescriptorLease`] 归还时只需还下标）。
pub(crate) struct DescriptorPool {
    device: ash::Device,
    pool: vk::DescriptorPool,
    chunks: Vec<[vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT]>,
    free: Vec<usize>,
}

impl DescriptorPool {
    /// 建池并一次性分配全部窗口段的 set（固定容量：窗口数有上限，不引入增长逻辑）。
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
        // SAFETY: 描述在本次调用期间有效；池在 Drop 里销毁。
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
        // SAFETY: 池刚创建，容量 = `set_count`；布局由 Pipeline 持有到池销毁之后。
        let sets = match unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
        } {
            Ok(sets) => sets,
            Err(error) => {
                // SAFETY: 池由本函数独占且尚未交给任何人。
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
            // 反序初始化：`pop` 先给出 0、1、2…（窗口段的分配顺序可预期）。
            free: (0..windows).rev().collect(),
        })
    }

    /// 取一个空闲段；`None` = 所有 `MAX_WINDOWS` 段都在使用。
    pub(crate) fn allocate(&mut self) -> Option<usize> {
        self.free.pop()
    }

    /// 归还一个段。
    pub(crate) fn free(&mut self, chunk: usize) {
        debug_assert!(
            chunk < self.chunks.len(),
            "descriptor chunk index must be in range"
        );
        self.free.push(chunk);
    }

    /// 某段的 `MAX_FRAMES_IN_FLIGHT` 个 set（按帧槽索引）。
    pub(crate) fn chunk(&self, chunk: usize) -> [vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT] {
        self.chunks[chunk]
    }

    /// 全部窗口段的全部 set（图集换代时重写图集绑定的广播面）。
    pub(crate) fn all_sets(&self) -> impl Iterator<Item = vk::DescriptorSet> + '_ {
        self.chunks.iter().flatten().copied()
    }
}

impl Drop for DescriptorPool {
    fn drop(&mut self) {
        // SAFETY: 所有 descriptor set 都从本池分配；销毁池会一并释放它们。
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
        // persist 的 layout 由 `GpuRenderer` 显式管理（进 = 出 = COLOR_ATTACHMENT_OPTIMAL）。
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
        // R11-2：`loadOp = LOAD` 会**读**上一帧写进 persist 的像素，`blend` 也会读 dst
        // （本引擎全局 `ONE / ONE_MINUS_SRC_ALPHA`）。跨 `vkQueueSubmit` 的写→读可见性必须由
        // memory dependency 建立：
        //   - `srcAccessMask` release 上一次提交的 color attachment 写；
        //   - `dstAccessMask` acquire 本帧的读（LOAD / blend）与写。
        // 旧实现的 `srcAccessMask = empty`、`dstAccessMask = WRITE` 只构成 execution dependency，
        // 不构成 memory dependency：在带压缩/tiling 元数据的实现（AMD DCC、Intel CCS、部分 Arm）
        // 上，增量帧会偶发旧值 / 噪声 / 整块颜色错，而全量帧完全正常 —— 正是「CI golden 偶发差异、
        // 手动复现不了」的形态。sync validation 会在这里报 `SYNC-HAZARD-READ_AFTER_WRITE`。
        .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
        .dst_access_mask(
            vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        )
        // `BY_REGION` 让驱动按 tile 处理依赖（CLEAR 通道多 acquire 一个 READ 没有额外成本）。
        .dependency_flags(vk::DependencyFlags::BY_REGION)];
    // SAFETY: 描述在本次调用期间有效。
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
    let pipelines = unsafe { device.create_graphics_pipelines(cache, &[create], None) };
    // shader module 建完管线即可销毁（管线已持有其副本）。
    // SAFETY: 两个 module 归本函数独占且已不被引用。
    unsafe {
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(ps, None);
    }
    pipelines
        .map_err(|(_, error)| init_error("create_graphics_pipelines", format!("{error:?}")))
        .map(|pipelines| pipelines[0])
}

/// 建一条 geometry PSO。
///
/// `stride` = 12 时 `POSITION` + 覆盖率距离（`Ui`；解析 AA 的 `float`）；16 时是
/// `POSITION` + `TEXCOORD`（`Textured`）。拓扑来自 `GeometryDesc.topology`（PSO 烘死状态），
/// 与 uber-quad 的差别只有顶点输入（`stride` 与属性表）与拓扑：其余状态（cull = NONE、
/// 无深度/模板、MSAA 关闭、`ONE / INV_SRC_ALPHA` 混合）逐字相同（`contract.md` §7）。
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
    // 12 B = `Ui`：第二个属性是解析 AA 的覆盖率距离（单个 float）。
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
    // SAFETY: `create` 的指针在本次调用期间有效；pipeline 在 Drop 里销毁。
    let pipelines = unsafe { device.create_graphics_pipelines(cache, &[create], None) };
    // SAFETY: 两个 module 归本函数独占且已不被引用。
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
    // SAFETY: `words` 是对齐后的本地 `Vec<u32>`，长度 = 代码字数；调用期间有效。
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

    /// R11-14：缓存文件的自定义头部必须携带**设备身份**（换卡 / 驱动升级后旧 blob 必须丢弃）。
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
        // 设备名长度 + 设备名。
        let name_len = header[28] as usize;
        assert_eq!(&header[29..29 + name_len], b"llvmpipe (LLVM 20.1.0)");
        assert_eq!(header.len(), 29 + name_len);

        // 头部不同（换卡 / 换驱动 / 换驱动版本）⇒ 前缀不匹配 ⇒ 丢弃。
        let other_uuid = [8u8; 16];
        let other = cache_header(&other_uuid, 0x1234_5678, "llvmpipe (LLVM 20.1.0)");
        assert!(!header.starts_with(&other));
        let other = cache_header(&uuid, 0x9999_9999, "llvmpipe (LLVM 20.1.0)");
        assert!(!header.starts_with(&other));
        let other = cache_header(&uuid, 0x1234_5678, "NVIDIA GeForce RTX 4090");
        assert!(!header.starts_with(&other));
    }

    /// 超长设备名按 `u8` 截断（头部长度字段只有 1 字节）。
    #[test]
    fn an_overlong_device_name_is_truncated() {
        let long = "x".repeat(4096);
        let header = cache_header(&[0u8; 16], 1, &long);
        assert_eq!(header[28], u8::MAX);
        assert_eq!(header.len(), 29 + u8::MAX as usize);
    }

    /// 去掉头部之后的 payload 正好是驱动 blob；越界给出空切片（不 panic）。
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

    /// 描述符池按窗口段预分配：段用尽后明确失败、归还后可复用。
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

        // 段的 set 数 = 帧槽数；归还的段可以被下一个窗口复用。
        assert_eq!(pool.chunk(chunks[0]).len(), MAX_FRAMES_IN_FLIGHT);
        pool.free(chunks[0]);
        assert_eq!(pool.allocate(), Some(chunks[0]));
        assert_eq!(
            pool.all_sets().count(),
            MAX_WINDOWS as usize * MAX_FRAMES_IN_FLIGHT
        );
    }
}

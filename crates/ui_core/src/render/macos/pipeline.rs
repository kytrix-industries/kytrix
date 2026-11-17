//! metallib 加载 + uber-quad / geometry 管线 + 采样器（`design/phase-12-macos-linux.md` §5.4）。
//!
//! 绑定模型与 P07 的 `contract.md` §7 逐字一致（**直绑，不做 argument buffer**，F-119）：
//! `buffer(0)` instances / `buffer(1)` clips / `buffer(2)` root / `texture(0)` glyph / `texture(1)` image /
//! `sampler(0)`。
//!
//! geometry 管线按 `GeometryPipeline` 分成三种（`Ui` / `Textured{Image}` / `Textured{Glyph}`），
//! 顶点布局由 pipeline 唯一决定（`contract.md` §7）。

use std::path::PathBuf;

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLColorWriteMask, MTLDevice, MTLFunction, MTLLibrary,
    MTLPixelFormat, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLSamplerAddressMode,
    MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerMipFilter, MTLSamplerState,
    MTLVertexDescriptor, MTLVertexFormat, MTLVertexStepFunction,
};

use crate::{
    error::EngineError,
    paint::{atlas::AtlasKind, geometry::GeometryPipeline},
    render::macos::device::{Device, init_error},
};

/// 两个 metallib 的编译期产物路径（`build.rs` 写进 `OUT_DIR/shaders`）。
const QUAD_METALLIB: &str = concat!(env!("OUT_DIR"), "/shaders/quad.metallib");
const GEOMETRY_METALLIB: &str = concat!(env!("OUT_DIR"), "/shaders/geometry.metallib");

/// `include_bytes!` 的占位产物前缀（`ui_core_SKIP_SHADERS=1` 或 host ≠ target）。
const SKIPPED: &[u8] = b"KYTRIX-SHADER-SKIPPED";

/// 加载后的 shader 库（uber-quad 一族 + geometry 一族）。
pub(crate) struct ShaderSet {
    quad: Retained<ProtocolObject<dyn MTLLibrary>>,
    geometry: Retained<ProtocolObject<dyn MTLLibrary>>,
}

impl ShaderSet {
    /// `OUT_DIR/shaders/*.metallib` → `newLibraryWithData:error:`（`dispatch2` 提供 `DispatchData`）。
    pub(crate) fn load(device: &Device) -> Result<Self, EngineError> {
        let quad = load_library(
            device,
            QUAD_METALLIB,
            include_bytes!(concat!(env!("OUT_DIR"), "/shaders/quad.metallib")),
        )?;
        let geometry = load_library(
            device,
            GEOMETRY_METALLIB,
            include_bytes!(concat!(env!("OUT_DIR"), "/shaders/geometry.metallib")),
        )?;
        Ok(Self { quad, geometry })
    }

    /// 取入口函数（缺失 ⇒ `ShaderLoad`，不静默跳过）。
    fn function(
        &self,
        library: &ProtocolObject<dyn MTLLibrary>,
        name: &str,
        path: &str,
    ) -> Result<Retained<ProtocolObject<dyn MTLFunction>>, EngineError> {
        library
            .newFunctionWithName(&objc2_foundation::NSString::from_str(name))
            .ok_or(EngineError::ShaderLoad {
                target: "metallib",
                path: PathBuf::from(format!("{path}#{name}")),
            })
    }
}

/// 加载一个 metallib（占位产物 ⇒ `ShaderLoad`）。
fn load_library(
    device: &Device,
    path: &str,
    bytes: &[u8],
) -> Result<Retained<ProtocolObject<dyn MTLLibrary>>, EngineError> {
    if bytes.starts_with(SKIPPED) {
        return Err(EngineError::ShaderLoad {
            target: "metallib",
            path: PathBuf::from(path),
        });
    }
    let data = dispatch2::DispatchData::from_bytes(bytes);
    device
        .raw()
        .newLibraryWithData_error(&data)
        .map_err(|_| EngineError::ShaderLoad {
            target: "metallib",
            path: PathBuf::from(path),
        })
}

/// uber-quad 管线 + geometry 管线 + 采样器。
pub(crate) struct Pipeline {
    quad: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_ui: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_image: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_glyph: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
}

impl Pipeline {
    pub(crate) fn new(device: &Device, format: MTLPixelFormat) -> Result<Self, EngineError> {
        let shaders = ShaderSet::load(device)?;
        let vs = shaders.function(&shaders.quad, "vs_main", QUAD_METALLIB)?;
        let fs = shaders.function(&shaders.quad, "ps_main", QUAD_METALLIB)?;
        let quad = build_pipeline(device, format, &vs, &fs, None)?;

        // geometry 三族：顶点布局由 stride 决定（`Ui` = 12 B，`Textured` = 16 B）。拓扑按
        // `GeometryDesc.topology` 各建一条 PSO（Metal 的拓扑是 PSO 状态，与 D3D12 的逐 draw 切换不同）。
        let ui_vs = shaders.function(&shaders.geometry, "vs_ui", GEOMETRY_METALLIB)?;
        let textured_vs = shaders.function(&shaders.geometry, "vs_textured", GEOMETRY_METALLIB)?;
        let ui_ps = shaders.function(&shaders.geometry, "ps_ui", GEOMETRY_METALLIB)?;
        let image_ps =
            shaders.function(&shaders.geometry, "ps_textured_image", GEOMETRY_METALLIB)?;
        let glyph_ps =
            shaders.function(&shaders.geometry, "ps_textured_glyph", GEOMETRY_METALLIB)?;
        let geometry_ui = build_pipeline(device, format, &ui_vs, &ui_ps, Some(12))?;
        let geometry_image = build_pipeline(device, format, &textured_vs, &image_ps, Some(16))?;
        let geometry_glyph = build_pipeline(device, format, &textured_vs, &glyph_ps, Some(16))?;

        let sampler_descriptor = MTLSamplerDescriptor::new();
        sampler_descriptor.setMinFilter(MTLSamplerMinMagFilter::Linear);
        sampler_descriptor.setMagFilter(MTLSamplerMinMagFilter::Linear);
        sampler_descriptor.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
        sampler_descriptor.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
        sampler_descriptor.setRAddressMode(MTLSamplerAddressMode::ClampToEdge);
        sampler_descriptor.setMipFilter(MTLSamplerMipFilter::NotMipmapped);
        sampler_descriptor.setLodMaxClamp(0.0);
        let sampler = device
            .raw()
            .newSamplerStateWithDescriptor(&sampler_descriptor)
            .ok_or_else(|| init_error("newSamplerState", "returned nil"))?;

        Ok(Self {
            quad,
            geometry_ui,
            geometry_image,
            geometry_glyph,
            sampler,
        })
    }

    pub(crate) fn quad(&self) -> &ProtocolObject<dyn MTLRenderPipelineState> {
        &self.quad
    }

    /// 按注册描述选 geometry PSO。
    pub(crate) fn geometry(
        &self,
        pipeline: GeometryPipeline,
    ) -> &ProtocolObject<dyn MTLRenderPipelineState> {
        match pipeline {
            GeometryPipeline::Ui => &self.geometry_ui,
            GeometryPipeline::Textured {
                atlas: AtlasKind::Image,
            } => &self.geometry_image,
            GeometryPipeline::Textured {
                atlas: AtlasKind::Glyph,
            } => &self.geometry_glyph,
        }
    }

    pub(crate) fn sampler(&self) -> &ProtocolObject<dyn MTLSamplerState> {
        &self.sampler
    }
}

/// 建一条 PSO：`vertex_stride = None` 是 uber-quad（无顶点缓冲），`Some(n)` 是 geometry。
fn build_pipeline(
    device: &Device,
    format: MTLPixelFormat,
    vs: &ProtocolObject<dyn MTLFunction>,
    fs: &ProtocolObject<dyn MTLFunction>,
    vertex_stride: Option<u32>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, EngineError> {
    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(vs));
    descriptor.setFragmentFunction(Some(fs));
    if let Some(stride) = vertex_stride {
        let layout = MTLVertexDescriptor::new();
        // SAFETY: 索引 0 恒存在（顶点描述符至少一个布局槽）；setStride / setOffset /
        // setBufferIndex 在 objc2 里标为 unsafe（描述符必须先由 layout/attributes 取到）。
        unsafe {
            let buffer = layout.layouts().objectAtIndexedSubscript(0);
            buffer.setStride(stride as usize);
            buffer.setStepFunction(MTLVertexStepFunction::PerVertex);
            let position = layout.attributes().objectAtIndexedSubscript(0);
            position.setFormat(MTLVertexFormat::Float2);
            position.setOffset(0);
            position.setBufferIndex(0);
            // 索引 1 恒存在（顶点描述符至少两个属性槽）。
            // 16 B = `Textured`（uv float2）；12 B = `Ui`（解析 AA 的覆盖率距离 float）。
            let second = layout.attributes().objectAtIndexedSubscript(1);
            if stride == 16 {
                second.setFormat(MTLVertexFormat::Float2);
            } else if stride == 12 {
                second.setFormat(MTLVertexFormat::Float);
            }
            second.setOffset(8);
            second.setBufferIndex(0);
        }
        descriptor.setVertexDescriptor(Some(&layout));
    }
    // 颜色附件：与 persist / drawable 同格式（sRGB 编码只由格式完成一次）。
    // SAFETY: 索引 0 恒存在（`MTLRenderPipelineDescriptor` 至少一个颜色附件槽）。
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(format);
    attachment.setBlendingEnabled(true);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setWriteMask(MTLColorWriteMask::All);
    device
        .raw()
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|error| {
            init_error(
                "newRenderPipelineState",
                error.localizedDescription().to_string(),
            )
        })
}

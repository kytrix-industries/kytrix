//! metallib 加载 + uber-quad 管线 + 采样器（`design/phase-12-macos-linux.md` §5.4）。
//!
//! 绑定模型与 P07 的 `contract.md` §7 逐字一致（**直绑，不做 argument buffer**，F-119）：
//! `buffer(0)` instances / `buffer(1)` clips / `buffer(2)` root / `texture(0)` glyph / `texture(1)` image /
//! `sampler(0)`。几何通道在 P12 没有注册入口（P08 同），故**不建** geometry PSO（F-134）。

use std::path::PathBuf;

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLColorWriteMask, MTLDevice, MTLFunction, MTLLibrary,
    MTLPixelFormat, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLSamplerAddressMode,
    MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerMipFilter, MTLSamplerState,
};

use crate::{
    error::EngineError,
    render::macos::device::{Device, init_error},
};

/// 两个 metallib 的编译期产物路径（`build.rs` 写进 `OUT_DIR/shaders`）。
const QUAD_METALLIB: &str = concat!(env!("OUT_DIR"), "/shaders/quad.metallib");

/// `include_bytes!` 的占位产物前缀（`ui_core_SKIP_SHADERS=1` 或 host ≠ target）。
const SKIPPED: &[u8] = b"KYTRIX-SHADER-SKIPPED";

/// 加载后的 shader 库（v1 只有 uber-quad 一族）。
pub(crate) struct ShaderSet {
    quad: Retained<ProtocolObject<dyn MTLLibrary>>,
}

impl ShaderSet {
    /// `OUT_DIR/shaders/quad.metallib` → `newLibraryWithData:error:`（`dispatch2` 提供 `DispatchData`）。
    pub(crate) fn load(device: &Device) -> Result<Self, EngineError> {
        let path = PathBuf::from(QUAD_METALLIB);
        let bytes: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders/quad.metallib"));
        if bytes.starts_with(SKIPPED) {
            return Err(EngineError::ShaderLoad {
                target: "metallib",
                path,
            });
        }
        let data = dispatch2::DispatchData::from_bytes(bytes);
        let library =
            device
                .raw()
                .newLibraryWithData_error(&data)
                .map_err(|_| EngineError::ShaderLoad {
                    target: "metallib",
                    path: path.clone(),
                })?;
        Ok(Self { quad: library })
    }

    /// 取入口函数（缺失 ⇒ `ShaderLoad`，不静默跳过）。
    fn function(
        &self,
        name: &str,
    ) -> Result<Retained<ProtocolObject<dyn MTLFunction>>, EngineError> {
        self.quad
            .newFunctionWithName(&objc2_foundation::NSString::from_str(name))
            .ok_or(EngineError::ShaderLoad {
                target: "metallib",
                path: PathBuf::from(format!("{QUAD_METALLIB}#{name}")),
            })
    }
}

/// uber-quad 管线 + 采样器。
pub(crate) struct Pipeline {
    quad: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
}

impl Pipeline {
    pub(crate) fn new(device: &Device, format: MTLPixelFormat) -> Result<Self, EngineError> {
        let shaders = ShaderSet::load(device)?;
        let vs = shaders.function("vs_main")?;
        let fs = shaders.function("ps_main")?;

        let descriptor = MTLRenderPipelineDescriptor::new();
        descriptor.setVertexFunction(Some(&vs));
        descriptor.setFragmentFunction(Some(&fs));
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
        let quad = device
            .raw()
            .newRenderPipelineStateWithDescriptor_error(&descriptor)
            .map_err(|error| {
                init_error(
                    "newRenderPipelineState",
                    error.localizedDescription().to_string(),
                )
            })?;

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

        Ok(Self { quad, sampler })
    }

    pub(crate) fn quad(&self) -> &ProtocolObject<dyn MTLRenderPipelineState> {
        &self.quad
    }

    pub(crate) fn sampler(&self) -> &ProtocolObject<dyn MTLSamplerState> {
        &self.sampler
    }
}

use std::path::PathBuf;

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLColorWriteMask, MTLDevice, MTLFunction, MTLLibrary,
    MTLPixelFormat, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLVertexDescriptor,
    MTLVertexFormat, MTLVertexStepFunction,
};

use crate::{
    error::EngineError,
    paint::{atlas::AtlasKind, geometry::GeometryPipeline},
    render::macos::device::init_error,
};

const QUAD_METALLIB: &str = concat!(env!("OUT_DIR"), "/shaders/quad.metallib");
const GEOMETRY_METALLIB: &str = concat!(env!("OUT_DIR"), "/shaders/geometry.metallib");

const SKIPPED: &[u8] = b"KYTRIX-SHADER-SKIPPED";

pub(crate) struct ShaderSet {
    quad: Retained<ProtocolObject<dyn MTLLibrary>>,
    geometry: Retained<ProtocolObject<dyn MTLLibrary>>,
}

impl ShaderSet {
    pub(crate) fn load(device: &ProtocolObject<dyn MTLDevice>) -> Result<Self, EngineError> {
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

    fn function(
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

fn load_library(
    device: &ProtocolObject<dyn MTLDevice>,
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
        .newLibraryWithData_error(&data)
        .map_err(|_| EngineError::ShaderLoad {
            target: "metallib",
            path: PathBuf::from(path),
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BlendMode {
    Premultiplied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PipelineLayout {
    Quad,
    GeometryUi,
    GeometryTextured(AtlasKind),
}

impl PipelineLayout {
    pub(crate) fn for_geometry(pipeline: GeometryPipeline) -> Self {
        match pipeline {
            GeometryPipeline::Ui => Self::GeometryUi,
            GeometryPipeline::Textured {
                atlas: AtlasKind::Image,
            } => Self::GeometryTextured(AtlasKind::Image),
            GeometryPipeline::Textured {
                atlas: AtlasKind::Glyph,
            } => Self::GeometryTextured(AtlasKind::Glyph),
        }
    }

    fn vertex_stride(self) -> Option<usize> {
        match self {
            Self::Quad => None,
            Self::GeometryUi => Some(12),
            Self::GeometryTextured(_) => Some(16),
        }
    }

    fn second_attribute(self) -> Option<MTLVertexFormat> {
        match self {
            Self::Quad => None,
            Self::GeometryUi => Some(MTLVertexFormat::Float),
            Self::GeometryTextured(_) => Some(MTLVertexFormat::Float2),
        }
    }

    fn functions(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Quad => (QUAD_METALLIB, "vs_main", "ps_main"),
            Self::GeometryUi => (GEOMETRY_METALLIB, "vs_ui", "ps_ui"),
            Self::GeometryTextured(AtlasKind::Image) => {
                (GEOMETRY_METALLIB, "vs_textured", "ps_textured_image")
            }
            Self::GeometryTextured(AtlasKind::Glyph) => {
                (GEOMETRY_METALLIB, "vs_textured", "ps_textured_glyph")
            }
        }
    }

    fn uses_geometry_library(self) -> bool {
        !matches!(self, Self::Quad)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PipelineKey {
    format: MTLPixelFormat,
    blend: BlendMode,
    layout: PipelineLayout,
}

pub(crate) struct PipelineSet {
    quad: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_ui: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_image: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_glyph: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

impl PipelineSet {
    pub(crate) fn keys(format: MTLPixelFormat, blend: BlendMode) -> [PipelineKey; 4] {
        [
            PipelineKey {
                format,
                blend,
                layout: PipelineLayout::Quad,
            },
            PipelineKey {
                format,
                blend,
                layout: PipelineLayout::for_geometry(GeometryPipeline::Ui),
            },
            PipelineKey {
                format,
                blend,
                layout: PipelineLayout::for_geometry(GeometryPipeline::Textured {
                    atlas: AtlasKind::Image,
                }),
            },
            PipelineKey {
                format,
                blend,
                layout: PipelineLayout::for_geometry(GeometryPipeline::Textured {
                    atlas: AtlasKind::Glyph,
                }),
            },
        ]
    }

    pub(crate) fn from_resolved(
        resolved: [Retained<ProtocolObject<dyn MTLRenderPipelineState>>; 4],
    ) -> Self {
        let [quad, geometry_ui, geometry_image, geometry_glyph] = resolved;
        Self {
            quad,
            geometry_ui,
            geometry_image,
            geometry_glyph,
        }
    }

    pub(crate) fn quad(&self) -> &ProtocolObject<dyn MTLRenderPipelineState> {
        &self.quad
    }

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

    #[cfg(test)]
    pub(crate) fn distinct_states(&self) -> bool {
        let pointers = [
            Retained::as_ptr(&self.quad),
            Retained::as_ptr(&self.geometry_ui),
            Retained::as_ptr(&self.geometry_image),
            Retained::as_ptr(&self.geometry_glyph),
        ];
        pointers
            .iter()
            .enumerate()
            .all(|(index, pointer)| !pointers[index + 1..].contains(pointer))
    }
}

pub(crate) fn build_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    shaders: &ShaderSet,
    key: PipelineKey,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, EngineError> {
    let (path, vertex_entry, fragment_entry) = key.layout.functions();
    let library = if key.layout.uses_geometry_library() {
        &shaders.geometry
    } else {
        &shaders.quad
    };
    let vs = ShaderSet::function(library, vertex_entry, path)?;
    let fs = ShaderSet::function(library, fragment_entry, path)?;
    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vs));
    descriptor.setFragmentFunction(Some(&fs));
    if let (Some(stride), Some(second_format)) =
        (key.layout.vertex_stride(), key.layout.second_attribute())
    {
        let layout = MTLVertexDescriptor::new();
        unsafe {
            let buffer = layout.layouts().objectAtIndexedSubscript(0);
            buffer.setStride(stride);
            buffer.setStepFunction(MTLVertexStepFunction::PerVertex);
            let position = layout.attributes().objectAtIndexedSubscript(0);
            position.setFormat(MTLVertexFormat::Float2);
            position.setOffset(0);
            position.setBufferIndex(0);
            let second = layout.attributes().objectAtIndexedSubscript(1);
            second.setFormat(second_format);
            second.setOffset(8);
            second.setBufferIndex(0);
        }
        descriptor.setVertexDescriptor(Some(&layout));
    }
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(key.format);
    match key.blend {
        BlendMode::Premultiplied => {
            attachment.setBlendingEnabled(true);
            attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
            attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            attachment.setRgbBlendOperation(MTLBlendOperation::Add);
            attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
            attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
        }
    }
    attachment.setWriteMask(MTLColorWriteMask::All);
    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|error| {
            init_error(
                "newRenderPipelineState",
                error.localizedDescription().to_string(),
            )
        })
}

#[cfg(test)]
mod tests {
    use objc2_metal::{MTLPixelFormat, MTLVertexFormat};

    use super::{BlendMode, PipelineKey, PipelineLayout, PipelineSet};
    use crate::paint::{atlas::AtlasKind, geometry::GeometryPipeline};

    fn key(layout: PipelineLayout) -> PipelineKey {
        PipelineKey {
            format: MTLPixelFormat::BGRA8Unorm_sRGB,
            blend: BlendMode::Premultiplied,
            layout,
        }
    }

    #[test]
    fn the_cache_key_separates_every_pso_input() {
        assert_eq!(key(PipelineLayout::Quad), key(PipelineLayout::Quad));
        assert_ne!(key(PipelineLayout::Quad), key(PipelineLayout::GeometryUi));
        assert_ne!(
            key(PipelineLayout::GeometryUi),
            key(PipelineLayout::GeometryTextured(AtlasKind::Glyph))
        );
        assert_ne!(
            key(PipelineLayout::GeometryTextured(AtlasKind::Image)),
            key(PipelineLayout::GeometryTextured(AtlasKind::Glyph))
        );
        let mut other = key(PipelineLayout::Quad);
        other.format = MTLPixelFormat::BGRA8Unorm;
        assert_ne!(other, key(PipelineLayout::Quad));
    }

    #[test]
    fn the_pipeline_set_keys_match_the_geometry_selector() {
        let keys = PipelineSet::keys(MTLPixelFormat::BGRA8Unorm_sRGB, BlendMode::Premultiplied);
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[0].layout, PipelineLayout::Quad);
        assert_eq!(
            keys[1].layout,
            PipelineLayout::for_geometry(GeometryPipeline::Ui)
        );
        assert_eq!(
            keys[2].layout,
            PipelineLayout::for_geometry(GeometryPipeline::Textured {
                atlas: AtlasKind::Image,
            })
        );
        assert_eq!(
            keys[3].layout,
            PipelineLayout::for_geometry(GeometryPipeline::Textured {
                atlas: AtlasKind::Glyph,
            })
        );
        for (index, left) in keys.iter().enumerate() {
            assert!(!keys[index + 1..].contains(left), "键 {left:?} 与后面重复");
        }
        assert_eq!(PipelineLayout::Quad.vertex_stride(), None);
        assert_eq!(PipelineLayout::GeometryUi.vertex_stride(), Some(12));
        assert_eq!(
            PipelineLayout::GeometryTextured(AtlasKind::Image).vertex_stride(),
            Some(16)
        );
        assert_eq!(
            PipelineLayout::GeometryUi.second_attribute(),
            Some(MTLVertexFormat::Float),
            "12 B 布局的第二个属性是标量覆盖率距离"
        );
        assert_eq!(
            PipelineLayout::GeometryTextured(AtlasKind::Glyph).second_attribute(),
            Some(MTLVertexFormat::Float2),
            "16 B 布局的第二个属性是 uv"
        );
        assert_eq!(PipelineLayout::Quad.second_attribute(), None);
    }
}

//! metallib 加载 + PSO 缓存键 + PSO 构建。
//!
//! 绑定模型与 P07 的 `contract.md` §7 逐字一致（**直绑，不做 argument buffer**，F-119）：
//! `buffer(0)` instances / `buffer(1)` clips / `buffer(2)` root / `texture(0)` glyph / `texture(1)` image /
//! `sampler(0)`。
//!
//! **本模块只产设备级对象**（R09-5 / R09-13）：两个 `MTLLibrary`（[`ShaderSet`]）与按
//! [`PipelineKey`] 缓存的 `MTLRenderPipelineState`。缓存与"进程内只加载一次"的所有权在
//! [`super::core::GpuCore`]（可替换的进程级注册表，见 `core.rs` 的「设备恢复」）；窗口级对象
//! 只剩 drawable / persist / 上传槽 / display link。设备恢复会换一个新 `GpuCore`（新
//! `MTLDevice` ⇒ 旧 PSO/ metallib 一律作废），新窗口的 PSO 在新 core 上按同一套键重新懒编译。
//!
//! PSO 缓存键 = **决定 PSO 的全部输入**：目标像素格式、颜色附件混合模式、顶点布局。
//!
//! - **拓扑不在键里**：Metal 的图元拓扑是 `drawIndexedPrimitives` 的参数（
//!   `MTLPrimitiveType`），不是 `MTLRenderPipelineDescriptor` 的状态 —— 同一份 PSO 能画
//!   `TriangleList` 与 `TriangleStrip`。把它放进键只会让同一份机器码被编译多次。
//! - 混合模式目前在共享契约里只有预乘 alpha 一种，但它确实是 PSO 状态，所以进键：将来加
//!   第二种混合时不会静默命中错误的 PSO。

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

/// 两个 metallib 的编译期产物路径（`build.rs` 写进 `OUT_DIR/shaders`）。
const QUAD_METALLIB: &str = concat!(env!("OUT_DIR"), "/shaders/quad.metallib");
const GEOMETRY_METALLIB: &str = concat!(env!("OUT_DIR"), "/shaders/geometry.metallib");

/// `include_bytes!` 的占位产物前缀（`ui_core_SKIP_SHADERS=1` 或 host ≠ target）。
const SKIPPED: &[u8] = b"KYTRIX-SHADER-SKIPPED";

/// 加载后的 shader 库（uber-quad 一族 + geometry 一族）。
///
/// 进程内只有一份（[`super::core::GpuCore`] 持有）：解析 metallib 是开窗路径上最贵的一段，
/// 没有理由每个窗口重做（R09-5）。
pub(crate) struct ShaderSet {
    quad: Retained<ProtocolObject<dyn MTLLibrary>>,
    geometry: Retained<ProtocolObject<dyn MTLLibrary>>,
}

impl ShaderSet {
    /// `OUT_DIR/shaders/*.metallib` → `newLibraryWithData:error:`（`dispatch2` 提供 `DispatchData`）。
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

    /// 取入口函数（缺失 ⇒ `ShaderLoad`，不静默跳过）。
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

/// 加载一个 metallib（占位产物 ⇒ `ShaderLoad`）。
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

/// 颜色附件的混合模式（PSO 状态之一）。
///
/// 共享契约只有一种：**预乘 alpha**（`One / OneMinusSourceAlpha`，RGB 与 A 同式），与
/// D3D12 的 `ONE / INV_SRC_ALPHA`、Vulkan 的 `src/one-minus-src-alpha` 逐位等价（§4.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BlendMode {
    /// 预乘 alpha。
    Premultiplied,
}

/// 顶点布局（PSO 状态之一；`Stride` 由它唯一决定，`contract.md` §7）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PipelineLayout {
    /// uber-quad：4 顶点 unit quad，**无**顶点缓冲（顶点由 `vertex_id` 生成）。
    Quad,
    /// geometry `Ui`：12 B/顶点 = position `float2` + 解析 AA 覆盖率距离 `float`。
    GeometryUi,
    /// geometry `Textured`：16 B/顶点 = position `float2` + uv `float2`；fragment 由图集种类决定。
    GeometryTextured(AtlasKind),
}

impl PipelineLayout {
    /// 注册描述 → 顶点布局（geometry 三族的唯一选择点）。
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

    /// 顶点缓冲步长；`None` = 没有顶点缓冲（uber-quad）。
    fn vertex_stride(self) -> Option<usize> {
        match self {
            Self::Quad => None,
            Self::GeometryUi => Some(12),
            Self::GeometryTextured(_) => Some(16),
        }
    }

    /// 第二个顶点属性的格式（12 B 布局是标量距离，16 B 布局是 uv）。
    fn second_attribute(self) -> Option<MTLVertexFormat> {
        match self {
            Self::Quad => None,
            Self::GeometryUi => Some(MTLVertexFormat::Float),
            Self::GeometryTextured(_) => Some(MTLVertexFormat::Float2),
        }
    }

    /// 顶点/片元入口函数名（两个 metallib 各自的命名空间）。
    fn functions(self) -> (&'static str, &'static str, &'static str) {
        // (metallib 路径, vertex 入口, fragment 入口)
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

    /// 该布局是否从 geometry metallib 取入口（否则来自 uber-quad）。
    fn uses_geometry_library(self) -> bool {
        !matches!(self, Self::Quad)
    }
}

/// PSO 缓存键：**决定 PSO 机器码的全部输入**。
///
/// 同键必然可以复用同一个 `MTLRenderPipelineState`；改任何一项都必须重新编译（见模块文档
/// 关于"为什么拓扑不进键"的说明）。字段对 crate 内其它模块不透明：只有本模块构造键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PipelineKey {
    /// 颜色附件（= persist / drawable）的像素格式。
    format: MTLPixelFormat,
    /// 颜色附件混合模式。
    blend: BlendMode,
    /// 顶点布局 + fragment 入口的选择。
    layout: PipelineLayout,
}

/// 一个渲染器需要的四条 PSO（从 [`super::core::GpuCore`] 的缓存里取，**不**在本类型里编译）。
///
/// 拿出来集中持有是为了让录制热路径不碰缓存的锁：`bind()` / 每个 geometry draw 都要取
/// PSO，每帧加一次全局锁没有必要。构造（= 首次解析）仍然走 [`super::core::GpuCore::pipeline_set`]，
/// 所以进程内每条 PSO 只编译一次。
pub(crate) struct PipelineSet {
    quad: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_ui: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_image: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_glyph: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

impl PipelineSet {
    /// 四条 PSO 对应的键（顺序：quad / ui / image / glyph）。
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

    /// 用四条已编译的 PSO 组装（调用方按 [`PipelineSet::keys`] 的顺序解析）。
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

    /// 当前缓存里的四条 PSO 互不相同（缓存没有把不同键折叠成同一个对象）。
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

/// 按 [`PipelineKey`] 编译一条 PSO。
///
/// `newRenderPipelineStateWithDescriptor:error:` 是**同步**的（首次要做后端机器码编译，数十
/// 毫秒）。它只应在进程第一次需要该键时执行 —— 调用点是
/// [`super::core::GpuCore::pipeline_set`]（由 GPU 线程在首帧录制时触发），因此不会阻塞主线程
/// （R09-5：开窗路径不再同步编译 PSO）。
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
        // SAFETY: 索引 0 恒存在（顶点描述符至少一个布局槽）；setStride / setOffset /
        // setBufferIndex 在 objc2 里标为 unsafe（描述符必须先由 layout/attributes 取到）。
        unsafe {
            let buffer = layout.layouts().objectAtIndexedSubscript(0);
            buffer.setStride(stride);
            buffer.setStepFunction(MTLVertexStepFunction::PerVertex);
            let position = layout.attributes().objectAtIndexedSubscript(0);
            position.setFormat(MTLVertexFormat::Float2);
            position.setOffset(0);
            position.setBufferIndex(0);
            // 索引 1 恒存在（顶点描述符至少两个属性槽）。
            let second = layout.attributes().objectAtIndexedSubscript(1);
            second.setFormat(second_format);
            second.setOffset(8);
            second.setBufferIndex(0);
        }
        descriptor.setVertexDescriptor(Some(&layout));
    }
    // 颜色附件：与 persist / drawable 同格式（sRGB 编码只由格式完成一次）。
    // SAFETY: 索引 0 恒存在（`MTLRenderPipelineDescriptor` 至少一个颜色附件槽）。
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

// 注册拓扑 → Metal 图元类型**不在本模块**：拓扑是 draw 参数而不是 PSO 状态，
// 由 `compositor` 的绘制路径直接映射（见模块文档）。

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

    /// 缓存键必须**区分每一个决定 PSO 的输入**：否则第二次请求会静默命中错误的机器码。
    #[test]
    fn the_cache_key_separates_every_pso_input() {
        assert_eq!(key(PipelineLayout::Quad), key(PipelineLayout::Quad));
        assert_ne!(key(PipelineLayout::Quad), key(PipelineLayout::GeometryUi));
        assert_ne!(
            key(PipelineLayout::GeometryUi),
            key(PipelineLayout::GeometryTextured(AtlasKind::Glyph))
        );
        // 同一顶点布局、不同图集 ⇒ 不同 fragment 入口 ⇒ 不同 PSO。
        assert_ne!(
            key(PipelineLayout::GeometryTextured(AtlasKind::Image)),
            key(PipelineLayout::GeometryTextured(AtlasKind::Glyph))
        );
        // 像素格式变化必须重新编译（sRGB 与非 sRGB 的写掩码/编码不同）。
        let mut other = key(PipelineLayout::Quad);
        other.format = MTLPixelFormat::BGRA8Unorm;
        assert_ne!(other, key(PipelineLayout::Quad));
    }

    /// `PipelineSet::keys` 与 `geometry()` 的选择必须一一对应：解析顺序写错会让 geometry
    /// 用上 quad 的 PSO（顶点布局不匹配 = Metal validation 报错或画错）。
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
        // 4 条键互不相同 ⇒ 4 条独立 PSO。
        for (index, left) in keys.iter().enumerate() {
            assert!(!keys[index + 1..].contains(left), "键 {left:?} 与后面重复");
        }
        // 每条键的顶点布局都能还原（12 B / 16 B / 无缓冲）。
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

//! 根签名 / quad PSO / geometry PSO / SRV 堆 / 静态几何表
//! （`design/phase-08-d3d12.md` §5.4；契约见 `render/shaders/contract.md`）。
//!
//! 逐字照 `contract.md` 与 P07 §2.9：拓扑 `TriangleStrip`（4 顶点、无顶点缓冲、`cull = NONE`）、
//! 混合 `ONE / INV_SRC_ALPHA`（RGBA 写掩码 0x0F）、`RTVFormats[0] = B8G8R8A8_UNORM_SRGB`
//! （**视图**格式，DEVIATIONS F-80）、根签名 28 个 root constant DWORD + 1 张 4-SRV 表 + 静态采样器。

use std::{ffi::CStr, mem::ManuallyDrop, path::PathBuf};

#[cfg(test)]
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
};
#[cfg(test)]
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R32_UINT};
use windows::{
    Win32::Graphics::{
        Direct3D::{D3D_PRIMITIVE_TOPOLOGY, ID3DBlob},
        Direct3D12::*,
        Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R32G32_FLOAT,
            DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
        },
    },
    core::PCSTR,
};

#[cfg(test)]
use crate::paint::geometry::{IndexType, Topology};
#[cfg(test)]
use crate::render::retire::GpuResourceHandle;
use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        geometry::{GeometryId, GeometryPipeline},
    },
    render::{
        frame::MAX_FRAMES_IN_FLIGHT,
        windows::device::{Device, init_error_from},
    },
};

/// 每个 in-flight 槽的 SRV 数：`[instances, clips, glyph, image]`。
pub(crate) const SLOT_SRVS: usize = 4;
const SLOT_COUNT: usize = MAX_FRAMES_IN_FLIGHT;
const SLOT_SPARE: usize = 2;
/// 堆容量：每槽 4 个在用 SRV + 2 个备用（S-75 的静态分区）。
const HEAP_CAPACITY: u32 = ((SLOT_SRVS + SLOT_SPARE) * SLOT_COUNT) as u32;

/// P07 产物（`build.rs` 写入 `OUT_DIR/shaders`；跳过模式是占位符）。
const QUAD_VS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders/quad.vs.dxil"));
const QUAD_PS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders/quad.ps.dxil"));
const GEOMETRY_VS_UI: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/shaders/geometry.vs_ui.dxil"));
const GEOMETRY_VS_TEXTURED: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/shaders/geometry.vs_textured.dxil"
));
const GEOMETRY_PS_UI: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/shaders/geometry.ps_ui.dxil"));
const GEOMETRY_PS_IMAGE: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/shaders/geometry.ps_textured_image.dxil"
));
const GEOMETRY_PS_GLYPH: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/shaders/geometry.ps_textured_glyph.dxil"
));

/// 一个 shader 产物 + 它的 `ShaderLoad` 路径（占位符 / 截断都算加载失败）。
fn shader(bytes: &'static [u8], name: &'static str) -> Result<&'static [u8], EngineError> {
    if bytes.starts_with(b"DXBC") {
        Ok(bytes)
    } else {
        Err(EngineError::ShaderLoad {
            target: "dxil",
            path: PathBuf::from(concat!(env!("OUT_DIR"), "/shaders/")).join(name),
        })
    }
}

/// 根签名 + 三个 PSO + shader-visible SRV 堆。
pub(crate) struct Pipeline {
    root_signature: ID3D12RootSignature,
    quad: ID3D12PipelineState,
    geometry_ui: ID3D12PipelineState,
    geometry_image: ID3D12PipelineState,
    geometry_glyph: ID3D12PipelineState,
    srv_heap: ID3D12DescriptorHeap,
    descriptor_size: u32,
}

impl Pipeline {
    pub(crate) fn new(device: &Device) -> Result<Self, EngineError> {
        let quad_vs = shader(QUAD_VS, "quad.vs.dxil")?;
        let quad_ps = shader(QUAD_PS, "quad.ps.dxil")?;
        let geometry_vs_ui = shader(GEOMETRY_VS_UI, "geometry.vs_ui.dxil")?;
        let geometry_vs_textured = shader(GEOMETRY_VS_TEXTURED, "geometry.vs_textured.dxil")?;
        let geometry_ps_ui = shader(GEOMETRY_PS_UI, "geometry.ps_ui.dxil")?;
        let geometry_ps_image = shader(GEOMETRY_PS_IMAGE, "geometry.ps_textured_image.dxil")?;
        let geometry_ps_glyph = shader(GEOMETRY_PS_GLYPH, "geometry.ps_textured_glyph.dxil")?;

        let root_signature = unsafe { build_root_signature(device) }?;

        let quad = unsafe {
            create_pipeline_state(
                device,
                &root_signature,
                quad_vs,
                quad_ps,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                None,
            )
        }?;
        let ui_layout = [input_element(c"POSITION", DXGI_FORMAT_R32G32_FLOAT, 0, 0)];
        let geometry_ui = unsafe {
            create_pipeline_state(
                device,
                &root_signature,
                geometry_vs_ui,
                geometry_ps_ui,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                Some(&ui_layout),
            )
        }?;
        let textured_layout = [
            input_element(c"POSITION", DXGI_FORMAT_R32G32_FLOAT, 0, 0),
            input_element(c"TEXCOORD", DXGI_FORMAT_R32G32_FLOAT, 0, 8),
        ];
        let geometry_image = unsafe {
            create_pipeline_state(
                device,
                &root_signature,
                geometry_vs_textured,
                geometry_ps_image,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                Some(&textured_layout),
            )
        }?;
        let geometry_glyph = unsafe {
            create_pipeline_state(
                device,
                &root_signature,
                geometry_vs_textured,
                geometry_ps_glyph,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                Some(&textured_layout),
            )
        }?;

        let srv_heap: ID3D12DescriptorHeap = unsafe {
            device
                .raw()
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: HEAP_CAPACITY,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
        }
        .map_err(|error| init_error_from("CreateDescriptorHeap(CBV_SRV_UAV)", error))?;
        let descriptor_size = unsafe {
            device
                .raw()
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
        };

        Ok(Self {
            root_signature,
            quad,
            geometry_ui,
            geometry_image,
            geometry_glyph,
            srv_heap,
            descriptor_size,
        })
    }

    pub(crate) fn heap(&self) -> &ID3D12DescriptorHeap {
        &self.srv_heap
    }

    pub(crate) fn root_signature(&self) -> &ID3D12RootSignature {
        &self.root_signature
    }

    /// uber-quad PSO。
    pub(crate) fn quad(&self) -> &ID3D12PipelineState {
        &self.quad
    }

    /// geometry PSO（`Textured` 按图集页选 PS）。
    pub(crate) fn geometry(&self, pipeline: GeometryPipeline) -> &ID3D12PipelineState {
        match pipeline {
            GeometryPipeline::Ui => &self.geometry_ui,
            GeometryPipeline::Textured { atlas } => match atlas {
                AtlasKind::Glyph => &self.geometry_glyph,
                AtlasKind::Image => &self.geometry_image,
            },
        }
    }

    /// 某槽的 4-SRV 表基址（root parameter 1 的描述符表起点）。
    pub(crate) fn slot_table(&self, slot: usize) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        let base = unsafe { self.srv_heap.GetGPUDescriptorHandleForHeapStart() };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + u64::from(self.descriptor_size) * (slot * SLOT_SRVS) as u64,
        }
    }

    /// 写某槽的 `[instances, clips]` SRV（图集 SRV 由 [`Pipeline::write_atlas`] 统一维护）。
    pub(crate) fn write_slot(
        &self,
        device: &Device,
        slot: usize,
        instances: &ID3D12Resource,
        instance_count: u32,
        clips: &ID3D12Resource,
        clip_count: u32,
    ) {
        let first = slot * SLOT_SRVS;
        // `NumElements = 0` 的 SRV 非法（本机实测会让设备丢失）：空帧至少给 1 个元素。
        let instances_srv = buffer_srv(instance_count.max(1), 128);
        let clips_srv = buffer_srv(clip_count.max(1), 48);
        unsafe {
            device.raw().CreateShaderResourceView(
                instances,
                Some(&instances_srv),
                self.cpu_handle(first),
            );
            device.raw().CreateShaderResourceView(
                clips,
                Some(&clips_srv),
                self.cpu_handle(first + 1),
            );
        }
    }

    /// 写图集 SRV（每个槽的 `[2]` / `[3]` 槽位，共 `SLOT_COUNT` 份）。
    pub(crate) fn write_atlas(&self, device: &Device, kind: AtlasKind, texture: &ID3D12Resource) {
        let offset = match kind {
            AtlasKind::Glyph => 2,
            AtlasKind::Image => 3,
        };
        let srv = texture_srv(kind);
        for slot in 0..SLOT_COUNT {
            let index = slot * SLOT_SRVS + offset;
            unsafe {
                device
                    .raw()
                    .CreateShaderResourceView(texture, Some(&srv), self.cpu_handle(index));
            }
        }
    }

    fn cpu_handle(&self, index: usize) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let base = unsafe { self.srv_heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + (self.descriptor_size * index as u32) as usize,
        }
    }
}

fn buffer_srv(num_elements: u32, stride: u32) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_UNKNOWN,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                FirstElement: 0,
                NumElements: num_elements,
                StructureByteStride: stride,
                Flags: D3D12_BUFFER_SRV_FLAG_NONE,
            },
        },
    }
}

fn texture_srv(kind: AtlasKind) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        // 字形图集是线性覆盖率，**不能**声明 sRGB（P07 §2.9）。
        Format: match kind {
            AtlasKind::Glyph => DXGI_FORMAT_R8_UNORM,
            AtlasKind::Image => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        },
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                PlaneSlice: 0,
                ResourceMinLODClamp: 0.0,
            },
        },
    }
}

fn input_element(
    name: &'static CStr,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    slot: u32,
    offset: u32,
) -> D3D12_INPUT_ELEMENT_DESC {
    D3D12_INPUT_ELEMENT_DESC {
        SemanticName: PCSTR::from_raw(name.as_ptr().cast()),
        SemanticIndex: 0,
        Format: format,
        InputSlot: slot,
        AlignedByteOffset: offset,
        InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
        InstanceDataStepRate: 0,
    }
}

unsafe fn build_root_signature(device: &Device) -> Result<ID3D12RootSignature, EngineError> {
    let ranges = [D3D12_DESCRIPTOR_RANGE {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
        NumDescriptors: SLOT_SRVS as u32,
        BaseShaderRegister: 0,
        RegisterSpace: 0,
        OffsetInDescriptorsFromTableStart: 0,
    }];

    let mut parameters = [D3D12_ROOT_PARAMETER::default(); 2];
    parameters[0].ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
    parameters[0].Anonymous = D3D12_ROOT_PARAMETER_0 {
        Constants: D3D12_ROOT_CONSTANTS {
            ShaderRegister: 0,
            RegisterSpace: 0,
            Num32BitValues: 28,
        },
    };
    parameters[0].ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
    parameters[1].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    parameters[1].Anonymous = D3D12_ROOT_PARAMETER_0 {
        DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
            NumDescriptorRanges: 1,
            pDescriptorRanges: ranges.as_ptr(),
        },
    };
    parameters[1].ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;

    let sampler = D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        BorderColor: D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
        MinLOD: 0.0,
        MaxLOD: 0.0,
        ShaderRegister: 0,
        RegisterSpace: 0,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
    };

    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: parameters.len() as u32,
        pParameters: parameters.as_ptr(),
        NumStaticSamplers: 1,
        pStaticSamplers: &sampler,
        Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
    };

    let mut blob: Option<ID3DBlob> = None;
    let mut error_blob: Option<ID3DBlob> = None;
    unsafe {
        D3D12SerializeRootSignature(
            &desc,
            D3D_ROOT_SIGNATURE_VERSION_1_0,
            &mut blob,
            Some(&mut error_blob),
        )
    }
    .map_err(|error| init_error_from("D3D12SerializeRootSignature", error))?;
    let blob = blob.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 SerializeRootSignature returned no blob"),
    })?;
    let bytes = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    };
    let _ = error_blob;
    unsafe { device.raw().CreateRootSignature(0, bytes) }
        .map_err(|error| init_error_from("CreateRootSignature", error))
}

unsafe fn create_pipeline_state(
    device: &Device,
    root_signature: &ID3D12RootSignature,
    vertex: &'static [u8],
    pixel: &'static [u8],
    topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    layout: Option<&[D3D12_INPUT_ELEMENT_DESC]>,
) -> Result<ID3D12PipelineState, EngineError> {
    let mut blend = D3D12_BLEND_DESC::default();
    blend.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        LogicOpEnable: false.into(),
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        RenderTargetWriteMask: 0x0F,
    };
    let depth = D3D12_DEPTH_STENCIL_DESC {
        DepthEnable: false.into(),
        StencilEnable: false.into(),
        DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        ..D3D12_DEPTH_STENCIL_DESC::default()
    };
    let mut rtv_formats = [DXGI_FORMAT_UNKNOWN; 8];
    rtv_formats[0] = DXGI_FORMAT_B8G8R8A8_UNORM_SRGB;
    let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root_signature.clone())),
        VS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: vertex.as_ptr().cast(),
            BytecodeLength: vertex.len(),
        },
        PS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: pixel.as_ptr().cast(),
            BytecodeLength: pixel.len(),
        },
        BlendState: blend,
        SampleMask: u32::MAX,
        RasterizerState: D3D12_RASTERIZER_DESC {
            FillMode: D3D12_FILL_MODE_SOLID,
            CullMode: D3D12_CULL_MODE_NONE,
            FrontCounterClockwise: false.into(),
            DepthBias: 0,
            DepthBiasClamp: 0.0,
            SlopeScaledDepthBias: 0.0,
            DepthClipEnable: true.into(),
            MultisampleEnable: false.into(),
            AntialiasedLineEnable: false.into(),
            ForcedSampleCount: 0,
            ConservativeRaster: D3D12_CONSERVATIVE_RASTERIZATION_MODE_OFF,
        },
        DepthStencilState: depth,
        InputLayout: match layout {
            Some(elements) => D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: elements.as_ptr(),
                NumElements: elements.len() as u32,
            },
            None => D3D12_INPUT_LAYOUT_DESC::default(),
        },
        IBStripCutValue: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
        PrimitiveTopologyType: topology,
        NumRenderTargets: 1,
        RTVFormats: rtv_formats,
        DSVFormat: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..D3D12_GRAPHICS_PIPELINE_STATE_DESC::default()
    };
    // SAFETY: desc 的字符串/字节切片指针指向本函数栈上的产物，CreateGraphicsPipelineState 同步消费。
    let state = unsafe { device.raw().CreateGraphicsPipelineState(&desc) }
        .map_err(|error| init_error_from("CreateGraphicsPipelineState", error));
    // SAFETY: `pRootSignature` 是 `ManuallyDrop`，desc 的 drop 不会释放 root signature 的 COM 引用
    // （DEVIATIONS F-86）；PSO 已经创建完成，这里显式释放。
    unsafe { ManuallyDrop::drop(&mut desc.pRootSignature) };
    state
}

/// 一条已注册静态几何的 GPU 视图（顶点/索引缓冲各持一个句柄）。
pub(crate) struct GeometryEntry {
    vertex: D3D12_VERTEX_BUFFER_VIEW,
    index: D3D12_INDEX_BUFFER_VIEW,
    index_count: u32,
    topology: D3D_PRIMITIVE_TOPOLOGY,
    pipeline: GeometryPipeline,
    /// 卸载时交还的句柄（P09 的 store 用；P08 只有测试会注册）。
    #[cfg(test)]
    storage: [GpuResourceHandle; 2],
}

impl GeometryEntry {
    pub(crate) fn vertex(&self) -> D3D12_VERTEX_BUFFER_VIEW {
        self.vertex
    }

    pub(crate) fn index(&self) -> D3D12_INDEX_BUFFER_VIEW {
        self.index
    }

    pub(crate) fn index_count(&self) -> u32 {
        self.index_count
    }

    pub(crate) fn topology(&self) -> D3D_PRIMITIVE_TOPOLOGY {
        self.topology
    }

    pub(crate) fn pipeline(&self) -> GeometryPipeline {
        self.pipeline
    }

    /// 卸载时交还的句柄（P09 的 store 用）。
    #[cfg(test)]
    pub(crate) fn storage(&self) -> [GpuResourceHandle; 2] {
        self.storage
    }
}

/// `GeometryId → GPU 缓冲`（P08 只有测试注册入口；公开的 `GeometryStore` 属 P09）。
pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,
}

impl GeometryRegistry {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub(crate) fn lookup(&self, id: GeometryId) -> Option<&GeometryEntry> {
        self.entries
            .get(id.index() as usize)
            .and_then(Option::as_ref)
    }

    /// 测试专用注册（P09 的 `GeometryStore` 是唯一公开铸造点；F-65）。
    #[cfg(test)]
    pub(crate) fn register(
        &mut self,
        device: &mut Device,
        id: GeometryId,
        vertex_bytes: &[u8],
        index_bytes: &[u8],
        pipeline: GeometryPipeline,
        topology: Topology,
        index_type: IndexType,
    ) -> Result<(), EngineError> {
        let vertex = upload_buffer(device, vertex_bytes)?;
        let index = upload_buffer(device, index_bytes)?;
        let vertex_location = unsafe { vertex.GetGPUVirtualAddress() };
        let index_location = unsafe { index.GetGPUVirtualAddress() };
        let vertex_handle = device.allocate(vertex);
        let index_handle = device.allocate(index);
        let index_format = match index_type {
            IndexType::U16 => DXGI_FORMAT_R16_UINT,
            IndexType::U32 => DXGI_FORMAT_R32_UINT,
        };
        let entry = GeometryEntry {
            vertex: D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: vertex_location,
                SizeInBytes: vertex_bytes.len() as u32,
                StrideInBytes: pipeline.vertex_stride(),
            },
            index: D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: index_location,
                SizeInBytes: index_bytes.len() as u32,
                Format: index_format,
            },
            index_count: (index_bytes.len() as u32) / index_type.size(),
            topology: match topology {
                Topology::TriangleList => D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
                Topology::TriangleStrip => D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            },
            pipeline,
            storage: [vertex_handle, index_handle],
        };
        let slot = id.index() as usize;
        if self.entries.len() <= slot {
            self.entries.resize_with(slot + 1, || None);
        }
        self.entries[slot] = Some(entry);
        Ok(())
    }
}

#[cfg(test)]
fn upload_buffer(device: &mut Device, bytes: &[u8]) -> Result<ID3D12Resource, EngineError> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.len().max(4) as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..D3D12_RESOURCE_DESC::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_UPLOAD,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let mut out: Option<ID3D12Resource> = None;
    unsafe {
        device.raw().CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
            &mut out,
        )
    }
    .map_err(|error| init_error_from("CreateCommittedResource(upload)", error))?;
    let resource = out.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 CreateCommittedResource returned no resource"),
    })?;
    let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
    unsafe { resource.Map(0, None, Some(&mut pointer)) }
        .map_err(|error| init_error_from("Map(upload)", error))?;
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len()) };
    unsafe { resource.Unmap(0, None) };
    Ok(resource)
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;

    use super::Pipeline;
    use crate::{
        paint::geometry::{GeometryId, GeometryPipeline, IndexType, Topology},
        render::windows::device::Device,
    };

    #[test]
    fn pipeline_builds_on_warp() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let device = Device::new_warp().expect("WARP device");
        let pipeline = Pipeline::new(&device).expect("pipeline");
        // 三张 PSO + 根签名的创建成功已由 `Pipeline::new` 的 `?` 保证；这里只查描述符表布局。
        let _ = pipeline.root_signature();
        let _ = pipeline.quad();
        let _ = pipeline.geometry(GeometryPipeline::Ui);
        assert_ne!(pipeline.slot_table(0).ptr, 0);
        assert_ne!(pipeline.slot_table(1).ptr, pipeline.slot_table(0).ptr);
    }

    #[test]
    fn geometry_registry_injects_a_ui_triangle() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut device = Device::new_warp().expect("WARP device");
        let mut registry = super::GeometryRegistry::new();
        let vertices = [0.0f32, 0.0, 32.0, 0.0, 0.0, 32.0]
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect::<Vec<u8>>();
        let indices = [0u16, 1, 2]
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect::<Vec<u8>>();
        registry
            .register(
                &mut device,
                GeometryId::for_test(0, 0),
                &vertices,
                &indices,
                GeometryPipeline::Ui,
                Topology::TriangleList,
                IndexType::U16,
            )
            .expect("register");
        let entry = registry.lookup(GeometryId::for_test(0, 0)).expect("entry");
        assert_eq!(entry.index_count(), 3);
        assert_eq!(entry.vertex().StrideInBytes, 8);
        assert_eq!(entry.topology(), D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        assert_eq!(entry.storage().len(), 2);
    }
}

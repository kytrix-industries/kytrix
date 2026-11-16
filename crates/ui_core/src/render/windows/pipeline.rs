//! 根签名 / quad PSO / geometry PSO / 描述符堆 / 静态几何表
//! （`design/phase-08-d3d12.md` §5.4；契约见 `render/shaders/contract.md`）。
//!
//! 进程级共享的图元只有 [`Pipeline`]（根签名 + 4 个 PSO），由 `device::SharedGpu` 创建一次。
//! 描述符全部在**一张** shader-visible 的 CBV/SRV/UAV 堆 [`SrvHeap`] 里（D3D12 规定每个
//! command list 每种堆最多只能绑一张，见 `SetDescriptorHeaps` 文档），布局分成两段：
//!
//! - 固定段 `[0, 2)`：`[glyph, image]` 图集 SRV。两张图集纹理是进程级共享资源，页重建时只重写
//!   这两个描述符（在 `SharedGpu` 的图集写锁 + 共享队列 idle 之后）。
//! - 窗口段：`[instances, clips]` per in-flight 槽，每窗口 `SLOT_COUNT × SLOT_SRVS` 个描述符，
//!   由 [`DescriptorArena`] 动态分配（[`crate::render::windows::device::DescriptorLease`] 持有，
//!   `Renderer` 销毁时归还）。实例 / 裁剪上传缓冲是每窗口对象（`UploadArena`），所以 SRV 也
//!   必须每窗口一份；窗口只写自己的描述符区间，互不加锁。
//!
//! 根签名因此有两张描述符表（同一张堆的两个区间）：`t0..t1`（窗口槽段）与 `t2..t3`（图集固定段）。
//! shader 的寄存器绑定不变（`common.hlsl`：`instances` t0 / `clips` t1 / `glyph_atlas` t2 /
//! `image_atlas` t3）。
//!
//! 拓扑 / 混合 / RTV 格式照 P07 §2.9：`TriangleStrip`（4 顶点、无顶点缓冲、`cull = NONE`）、
//! 混合 `ONE / INV_SRC_ALPHA`（RGBA 写掩码 0x0F）、`RTVFormats[0] = B8G8R8A8_UNORM_SRGB`
//! （**视图**格式，DEVIATIONS F-80）、根签名 28 个 root constant DWORD + 静态采样器。

use std::{ffi::CStr, mem::ManuallyDrop, path::PathBuf};

use windows::{
    Win32::Graphics::{
        Direct3D::{
            D3D_PRIMITIVE_TOPOLOGY, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob,
        },
        Direct3D12::*,
        Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R16_UINT,
            DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
        },
    },
    core::PCSTR,
};

use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        geometry::{GeometryId, GeometryPipeline, IndexType, Topology},
    },
    render::{
        frame::MAX_FRAMES_IN_FLIGHT, retire::GpuResourceHandle, windows::device::init_error_from,
    },
};

/// 每个 in-flight 槽在窗口段里的 SRV 数：`[instances, clips]`。
const SLOT_SRVS: u32 = 2;
const SLOT_COUNT: u32 = MAX_FRAMES_IN_FLIGHT as u32;
/// 每窗口占用的描述符数（in-flight 槽 × 每槽 SRV）。
const WINDOW_SRVS: u32 = SLOT_SRVS * SLOT_COUNT;
/// 固定段：`[glyph, image]` 图集 SRV。
const ATLAS_SRVS: u32 = 2;
/// 堆容量：固定段 + 341 个窗口（每描述符 32 B ⇒ 约 64 KiB，相对显存可忽略）。
const SRV_HEAP_CAPACITY: u32 = 2048;
/// 最多同时存在的窗口数（由堆容量决定；超出时创建渲染器返回 `AppInit`）。
pub(crate) const MAX_WINDOWS: u32 = (SRV_HEAP_CAPACITY - ATLAS_SRVS) / WINDOW_SRVS;

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

/// 进程级共享：根签名 + 三个 PSO（创建一次，所有窗口复用）。
pub(crate) struct Pipeline {
    root_signature: ID3D12RootSignature,
    quad: ID3D12PipelineState,
    geometry_ui: ID3D12PipelineState,
    geometry_image: ID3D12PipelineState,
    geometry_glyph: ID3D12PipelineState,
}

impl Pipeline {
    pub(crate) fn new(device: &ID3D12Device) -> Result<Self, EngineError> {
        let quad_vs = shader(QUAD_VS, "quad.vs.dxil")?;
        let quad_ps = shader(QUAD_PS, "quad.ps.dxil")?;
        let geometry_vs_ui = shader(GEOMETRY_VS_UI, "geometry.vs_ui.dxil")?;
        let geometry_vs_textured = shader(GEOMETRY_VS_TEXTURED, "geometry.vs_textured.dxil")?;
        let geometry_ps_ui = shader(GEOMETRY_PS_UI, "geometry.ps_ui.dxil")?;
        let geometry_ps_image = shader(GEOMETRY_PS_IMAGE, "geometry.ps_textured_image.dxil")?;
        let geometry_ps_glyph = shader(GEOMETRY_PS_GLYPH, "geometry.ps_textured_glyph.dxil")?;

        // SAFETY: 设备句柄由调用方持有，调用期间有效；错误按 HRESULT / `Result` 处理。
        let root_signature = unsafe { build_root_signature(device) }?;

        // SAFETY: 设备 / 根签名句柄由调用方持有，调用期间有效；错误按 HRESULT 处理。
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
        // SAFETY: 同上。
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
        // SAFETY: 同上。
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
        // SAFETY: 同上。
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

        Ok(Self {
            root_signature,
            quad,
            geometry_ui,
            geometry_image,
            geometry_glyph,
        })
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
}

/// 进程级共享的 shader-visible CBV/SRV/UAV 堆。
///
/// 布局（描述符索引）：`[0, ATLAS_SRVS)` 是图集固定段（glyph @0、image @1）；之后是每窗口段
/// （`[instances, clips]` × in-flight 槽）。窗口段由 [`DescriptorArena`] 分配，窗口只写自己的
/// 区间。D3D12 每个 command list 每种堆只能绑一张（`SetDescriptorHeaps` 文档），所以实例 / 裁剪
/// 与图集必须共用这一张堆 —— 根签名的两张描述符表指向同一张堆的两个区间。
pub(crate) struct SrvHeap {
    heap: ID3D12DescriptorHeap,
    descriptor_size: u32,
}

impl SrvHeap {
    pub(crate) fn new(device: &ID3D12Device) -> Result<Self, EngineError> {
        // SAFETY: 设备句柄由调用方持有，调用期间有效；错误按 HRESULT 返回。
        let heap: ID3D12DescriptorHeap = unsafe {
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: SRV_HEAP_CAPACITY,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
        }
        .map_err(|error| init_error_from("CreateDescriptorHeap(CBV_SRV_UAV)", error))?;
        // SAFETY: 堆由 `SrvHeap` 持有，句柄在堆生命周期内有效；查询无副作用。
        let descriptor_size = unsafe {
            device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
        };
        Ok(Self {
            heap,
            descriptor_size,
        })
    }

    pub(crate) fn heap(&self) -> &ID3D12DescriptorHeap {
        &self.heap
    }

    /// 图集表基址（root parameter 2；glyph @0、image @1）。
    pub(crate) fn atlas_table(&self) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        self.gpu_handle(0)
    }

    /// 某窗口某槽的 `[instances, clips]` 表基址（root parameter 1）。
    pub(crate) fn slot_table(&self, window_base: u32, slot: usize) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        self.gpu_handle(window_base + slot as u32 * SLOT_SRVS)
    }

    /// 写某窗口某槽的 `[instances, clips]` SRV。
    pub(crate) fn write_slot(
        &self,
        device: &ID3D12Device,
        window_base: u32,
        slot: usize,
        instances: &ID3D12Resource,
        instance_count: u32,
        clips: &ID3D12Resource,
        clip_count: u32,
    ) {
        // `NumElements = 0` 的 SRV 非法（本机实测会让设备丢失）：空帧至少给 1 个元素。
        let instances_srv = buffer_srv(instance_count.max(1), 128);
        let clips_srv = buffer_srv(clip_count.max(1), 48);
        let first = window_base + slot as u32 * SLOT_SRVS;
        // SAFETY: 设备 / 堆句柄由调用方持有，调用期间有效；描述符区间在容量内。
        unsafe {
            device.CreateShaderResourceView(
                instances,
                Some(&instances_srv),
                self.cpu_handle(first),
            );
            device.CreateShaderResourceView(clips, Some(&clips_srv), self.cpu_handle(first + 1));
        }
    }

    /// 写一个图集的 SRV（另一个图集的描述符保持不变）。
    pub(crate) fn write_atlas(
        &self,
        device: &ID3D12Device,
        kind: AtlasKind,
        texture: &ID3D12Resource,
    ) {
        let srv = texture_srv(kind);
        // SAFETY: 设备 / 堆句柄由调用方持有，调用期间有效；描述符索引在固定段内。
        unsafe {
            device.CreateShaderResourceView(texture, Some(&srv), self.cpu_handle(kind.index()));
        }
    }

    fn cpu_handle(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        // SAFETY: 描述符堆由本结构持有，句柄在堆生命周期内有效。
        let base = unsafe { self.heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + (self.descriptor_size * index) as usize,
        }
    }

    fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: 描述符堆由本结构持有，句柄在堆生命周期内有效。
        let base = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart() };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + u64::from(self.descriptor_size) * u64::from(index),
        }
    }
}

/// 共享 SRV 堆的窗口段分配器：每窗口 `WINDOW_SRVS` 个描述符，`Renderer` 销毁时归还。
pub(crate) struct DescriptorArena {
    next: u32,
    free: Vec<u32>,
}

impl DescriptorArena {
    pub(crate) const fn new() -> Self {
        Self {
            next: 0,
            free: Vec::new(),
        }
    }

    /// 分配一个窗口段，返回段内第一个描述符的**绝对索引**。
    pub(crate) fn allocate(&mut self) -> Option<u32> {
        if let Some(index) = self.free.pop() {
            return Some(ATLAS_SRVS + index * WINDOW_SRVS);
        }
        if self.next < MAX_WINDOWS {
            let index = self.next;
            self.next += 1;
            return Some(ATLAS_SRVS + index * WINDOW_SRVS);
        }
        None
    }

    /// 归还一个窗口段（`base` 是 [`SrvHeap::slot_table`] 用的段起始索引）。
    pub(crate) fn free(&mut self, base: u32) {
        debug_assert!(base >= ATLAS_SRVS);
        debug_assert_eq!(
            (base - ATLAS_SRVS) % WINDOW_SRVS,
            0,
            "window descriptor base must be aligned"
        );
        self.free.push((base - ATLAS_SRVS) / WINDOW_SRVS);
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

unsafe fn build_root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature, EngineError> {
    // 两张表（同一张共享堆的两个区间）：窗口槽段（t0..t1）与图集固定段（t2..t3）。
    let ranges = [
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: SLOT_SRVS,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 0,
        },
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: ATLAS_SRVS,
            BaseShaderRegister: SLOT_SRVS,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 0,
        },
    ];

    let mut parameters = [D3D12_ROOT_PARAMETER::default(); 3];
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
            pDescriptorRanges: &ranges[0],
        },
    };
    parameters[1].ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
    parameters[2].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    parameters[2].Anonymous = D3D12_ROOT_PARAMETER_0 {
        DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
            NumDescriptorRanges: 1,
            pDescriptorRanges: &ranges[1],
        },
    };
    parameters[2].ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;

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
    // SAFETY: 描述里的指针都指向本函数栈上的数组，调用期间有效。
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
    // SAFETY: blob 由本函数持有，字节区间在调用期间有效。
    let bytes = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    };
    let _ = error_blob;
    // SAFETY: 设备句柄由调用方持有，调用期间有效；错误按 HRESULT 处理。
    unsafe { device.CreateRootSignature(0, bytes) }
        .map_err(|error| init_error_from("CreateRootSignature", error))
}

unsafe fn create_pipeline_state(
    device: &ID3D12Device,
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
    let state = unsafe { device.CreateGraphicsPipelineState(&desc) }
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
    /// 该条目对应的代际（槽位复用后必须重建缓冲，否则会画成新几何）。
    generation: u32,
    /// 最后一次被绘制的帧号（GeometryRegistry 的单调帧号）。
    ///
    /// 用于驱逐：应用释放几何句柄后本窗口的 GPU 缓冲仍会驻留，必须按「最近使用」回收，
    /// 否则「每帧注册新几何、释放旧的」会让显存无界增长（D-1）。
    last_used_frame: u64,
    /// 卸载时交还的句柄。
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

    /// 卸载时交还的句柄。
    pub(crate) fn storage(&self) -> [GpuResourceHandle; 2] {
        self.storage
    }
}

/// `GeometryId → GPU 缓冲`（按需创建；代际不匹配即重建）。
///
/// 缓冲由 GPU 线程在**首次见到某个 `GeometryId`** 时创建：编码器把注册描述附在
/// `GeometryDrawGpu` 上随帧过线，因此这里不需要额外的跨线程注册通道。
/// 槽位复用（`generation` 变化）会**丢弃并重建**该槽的缓冲，避免旧图元画成新几何。
///
/// 每窗口一份：几何缓冲只被本窗口的命令列表引用，退役句柄走本窗口的 retire 队列，因此不需要
/// 跨窗口共享（共享它们会引入「哪个窗口的 fence 先到达」的所有权问题，见 D-1）。
pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,
    /// 单调帧号：每个内容帧录制前自增，`last_used_frame` 与驱逐都基于它。
    frame: u64,
}

/// 连续多少帧未被绘制就回收该几何的 GPU 缓冲。
///
/// 180 帧 ≈ 60 Hz 下的 3 秒：足够覆盖「隔帧 / 低频动画」的几何，同时让被应用释放的静态
/// 几何在几秒内归还显存（Chrome 的 discardable GPU cache 用同类按使用时间的策略）。
const EVICT_AFTER_FRAMES: u64 = 180;

/// 该几何是否已连续 EVICT_AFTER_FRAMES 帧以上未被绘制（wrapping_sub 容忍回绕）。
fn geometry_is_stale(frame: u64, last_used_frame: u64) -> bool {
    frame.wrapping_sub(last_used_frame) > EVICT_AFTER_FRAMES
}

impl GeometryRegistry {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
            frame: 0,
        }
    }

    /// 推进帧号（每个内容帧录制前调用一次）。
    pub(crate) fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// 驱逐长期未绘制的几何，把设备对象交给调用方 retire。
    pub(crate) fn retain_recent(&mut self, retired: &mut Vec<GpuResourceHandle>) {
        let frame = self.frame;
        for slot in &mut self.entries {
            let stale = slot
                .as_ref()
                .is_some_and(|entry| geometry_is_stale(frame, entry.last_used_frame));
            if stale && let Some(entry) = slot.take() {
                retired.extend(entry.storage());
            }
        }
    }

    /// teardown：把本窗口几何缓冲的句柄交回共享设备（GPU 线程已 idle，列表已重置）。
    pub(crate) fn release(&self, shared: &super::device::SharedGpu) {
        for entry in self.entries.iter().flatten() {
            for handle in entry.storage() {
                let _ = shared.release(handle);
            }
        }
    }

    /// 取该句柄的 GPU 缓冲；没有或代际不匹配时按 `desc` 现场创建。
    ///
    /// `retired` 收集被替换掉的旧句柄（调用方在录制块结束后交给 retire 队列，I4）。
    pub(crate) fn ensure(
        &mut self,
        shared: &super::device::SharedGpu,
        id: GeometryId,
        desc: &crate::paint::geometry::GeometryDesc,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<&GeometryEntry, EngineError> {
        let slot = id.index() as usize;
        if self.entries.len() <= slot {
            self.entries.resize_with(slot + 1, || None);
        }
        let stale = match &self.entries[slot] {
            Some(entry) => entry.generation != id.generation(),
            None => true,
        };
        if stale {
            if let Some(old) = self.entries[slot].take() {
                retired.extend(old.storage());
            }
            let entry = upload_geometry(shared, id, desc)?;
            self.entries[slot] = Some(entry);
        }
        // 记录使用帧号：`retain_recent` 据此驱逐长期未绘制的几何（D-1）。
        if let Some(entry) = &mut self.entries[slot] {
            entry.last_used_frame = self.frame;
        }
        self.entries[slot]
            .as_ref()
            .ok_or_else(|| EngineError::GeometryRegister {
                detail: smol_str::format_smolstr!(
                    "geometry {}:{} failed to produce GPU buffers",
                    id.index(),
                    id.generation()
                ),
            })
    }
}

/// 按注册描述创建顶点/索引缓冲（`UPLOAD` 堆；几何是静态数据，装载期一次写入）。
fn upload_geometry(
    shared: &super::device::SharedGpu,
    id: GeometryId,
    desc: &crate::paint::geometry::GeometryDesc,
) -> Result<GeometryEntry, EngineError> {
    let vertex = upload_buffer(shared, &desc.vertex_bytes)?;
    let index = upload_buffer(shared, &desc.index_bytes)?;
    // SAFETY: 缓冲由 `GeometryRegistry` 持有，提交前不会释放；地址只在录制期使用。
    let vertex_location = unsafe { vertex.GetGPUVirtualAddress() };
    // SAFETY: 同上。
    let index_location = unsafe { index.GetGPUVirtualAddress() };
    let vertex_handle = shared.allocate(vertex);
    let index_handle = shared.allocate(index);
    let index_format = match desc.index_type {
        IndexType::U16 => DXGI_FORMAT_R16_UINT,
        IndexType::U32 => DXGI_FORMAT_R32_UINT,
    };
    Ok(GeometryEntry {
        vertex: D3D12_VERTEX_BUFFER_VIEW {
            BufferLocation: vertex_location,
            SizeInBytes: desc.vertex_bytes.len() as u32,
            StrideInBytes: desc.pipeline.vertex_stride(),
        },
        index: D3D12_INDEX_BUFFER_VIEW {
            BufferLocation: index_location,
            SizeInBytes: desc.index_bytes.len() as u32,
            Format: index_format,
        },
        index_count: (desc.index_bytes.len() as u32) / desc.index_type.size(),
        topology: match desc.topology {
            Topology::TriangleList => D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            Topology::TriangleStrip => D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        },
        pipeline: desc.pipeline,
        generation: id.generation(),
        // 由 `ensure` 在返回前立刻写成当前帧号；这里只是构造初值。
        last_used_frame: 0,
        storage: [vertex_handle, index_handle],
    })
}

fn upload_buffer(
    shared: &super::device::SharedGpu,
    bytes: &[u8],
) -> Result<ID3D12Resource, EngineError> {
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
    // SAFETY: COM 调用：设备句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
    unsafe {
        shared.raw().CreateCommittedResource(
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
    // SAFETY: 映射区间由 `Map` 独占，直到同一函数的 `Unmap`；容量由该缓冲的创建尺寸保证。
    unsafe { resource.Map(0, None, Some(&mut pointer)) }
        .map_err(|error| init_error_from("Map(upload)", error))?;
    // SAFETY: `pointer` 容量 ≥ `bytes.len()`（缓冲按该长度创建），映射期间无并发访问。
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len()) };
    // SAFETY: `Unmap` 与同一函数的 `Map` 成对，映射期间无并发访问。
    unsafe { resource.Unmap(0, None) };
    Ok(resource)
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;

    use super::{ATLAS_SRVS, DescriptorArena, Pipeline, SrvHeap, WINDOW_SRVS};
    use crate::{
        paint::geometry::{GeometryId, GeometryPipeline, IndexType, Topology},
        render::windows::device::Device,
    };

    #[test]
    fn pipeline_builds_on_warp() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let device = Device::new_warp().expect("WARP device");
        let pipeline = Pipeline::new(device.raw()).expect("pipeline");
        // 4 张 PSO + 根签名的创建成功已由 `Pipeline::new` 的 `?` 保证；这里只查句柄可访问。
        let _ = pipeline.root_signature();
        let _ = pipeline.quad();
        let _ = pipeline.geometry(GeometryPipeline::Ui);
    }

    /// 一张共享堆里的两段：图集固定段在索引 0/1，窗口段从 `ATLAS_SRVS` 开始。
    /// 驱逐决策的边界（不建真实设备：只验「多少帧未用才算过期」这一纯逻辑）。
    #[test]
    fn geometry_is_evicted_only_after_the_grace_window() {
        use super::{EVICT_AFTER_FRAMES, geometry_is_stale};

        // 刚用过：一定保留。
        assert!(!geometry_is_stale(1_000, 1_000));
        // 刚好差到门槛：保留（判据是 `>` 而不是 `>=`）。
        assert!(!geometry_is_stale(1_000 + EVICT_AFTER_FRAMES, 1_000));
        // 越过门槛：驱逐。
        assert!(geometry_is_stale(1_001 + EVICT_AFTER_FRAMES, 1_000));
        // 帧号回绕（u64 实际不会发生，但 `wrapping_sub` 必须给出正确差值）。
        assert!(!geometry_is_stale(0, u64::MAX));
    }

    #[test]
    fn the_srv_heap_reserves_the_atlas_segment_before_window_segments() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let device = Device::new_warp().expect("WARP device");
        let heap = SrvHeap::new(device.raw()).expect("srv heap");
        let mut arena = DescriptorArena::new();
        let first = arena.allocate().expect("first window segment");
        let second = arena.allocate().expect("second window segment");
        assert_eq!(first, ATLAS_SRVS);
        assert_eq!(second, ATLAS_SRVS + WINDOW_SRVS);
        assert_ne!(heap.atlas_table().ptr, 0);
        assert_ne!(heap.slot_table(first, 0).ptr, heap.atlas_table().ptr);
        assert_ne!(heap.slot_table(first, 1).ptr, heap.slot_table(first, 0).ptr);

        // 归还的段会被复用（窗口关闭后新窗口拿回同一段）。
        arena.free(first);
        assert_eq!(arena.allocate(), Some(first));
    }

    /// 窗口段分配器在容量耗尽时返回 `None`（不越界写共享堆）。
    #[test]
    fn the_descriptor_arena_stops_at_the_heap_capacity() {
        let mut arena = DescriptorArena::new();
        let mut allocated = 0;
        while let Some(base) = arena.allocate() {
            assert!(base + WINDOW_SRVS <= super::SRV_HEAP_CAPACITY);
            allocated += 1;
            assert!(allocated <= super::MAX_WINDOWS);
        }
        assert_eq!(allocated, super::MAX_WINDOWS);
    }

    #[test]
    fn geometry_registry_injects_a_ui_triangle() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let shared = super::super::device::SharedGpu::new_warp().expect("WARP shared gpu");
        let mut registry = super::GeometryRegistry::new();
        let vertices = [0.0f32, 0.0, 32.0, 0.0, 0.0, 32.0]
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect::<Vec<u8>>();
        let indices = [0u16, 1, 2]
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect::<Vec<u8>>();
        let desc = crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::copy_from_slice(&vertices),
            index_bytes: bytes::Bytes::copy_from_slice(&indices),
            vertex_stride: GeometryPipeline::Ui.vertex_stride(),
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: crate::math::Rect::new(0.0, 0.0, 32.0, 32.0),
            pipeline: GeometryPipeline::Ui,
        };
        let id = GeometryId::for_test(0, 1);
        let mut retired = Vec::new();
        let entry = registry
            .ensure(&shared, id, &desc, &mut retired)
            .expect("ensure");
        assert_eq!(entry.index_count(), 3);
        assert_eq!(entry.vertex().StrideInBytes, 8);
        assert_eq!(entry.topology(), D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        assert_eq!(entry.storage().len(), 2);
        assert!(retired.is_empty(), "首次创建不产生退役句柄");

        // 同一 id + 同一代际：命中缓存，不重建、不产生退役句柄。
        let again = registry
            .ensure(&shared, id, &desc, &mut retired)
            .expect("cached");
        assert_eq!(again.index_count(), 3);
        assert!(retired.is_empty());

        // 槽位复用（代际 +1）：必须重建，并把旧句柄交还 retire。
        let reused = GeometryId::for_test(0, 2);
        let _ = registry
            .ensure(&shared, reused, &desc, &mut retired)
            .expect("rebuilt");
        assert_eq!(retired.len(), 2, "换代必须退役旧顶点/索引缓冲");
    }
}

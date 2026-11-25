use std::{ffi::CStr, mem::ManuallyDrop, path::PathBuf};

use windows::{
    Win32::Graphics::{
        Direct3D::{
            D3D_PRIMITIVE_TOPOLOGY, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob,
        },
        Direct3D12::*,
        Dxgi::Common::{
            DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8_UNORM,
            DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R32_FLOAT,
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

const SLOT_SRVS: u32 = 2;
const SLOT_COUNT: u32 = MAX_FRAMES_IN_FLIGHT as u32;
const WINDOW_SRVS: u32 = SLOT_SRVS * SLOT_COUNT;
const ATLAS_SRVS: u32 = 3;
pub(crate) const ATLAS_GENERATIONS: u8 = 2;
const ATLAS_DESCRIPTORS: u32 = ATLAS_SRVS * ATLAS_GENERATIONS as u32;
const SRV_HEAP_CAPACITY: u32 = 2048;
pub(crate) const MAX_WINDOWS: u32 = (SRV_HEAP_CAPACITY - ATLAS_DESCRIPTORS) / WINDOW_SRVS;

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
        let ui_layout = [
            input_element(c"POSITION", DXGI_FORMAT_R32G32_FLOAT, 0, 0),
            input_element(c"TEXCOORD", DXGI_FORMAT_R32_FLOAT, 0, 8),
        ];
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

    pub(crate) fn quad(&self) -> &ID3D12PipelineState {
        &self.quad
    }

    pub(crate) fn geometry(&self, pipeline: GeometryPipeline) -> &ID3D12PipelineState {
        match pipeline {
            GeometryPipeline::Ui => &self.geometry_ui,
            GeometryPipeline::Textured { atlas } => match atlas {
                AtlasKind::Glyph | AtlasKind::ColorGlyph => &self.geometry_glyph,
                AtlasKind::Image => &self.geometry_image,
            },
        }
    }
}

pub(crate) struct SrvHeap {
    heap: ID3D12DescriptorHeap,
    descriptor_size: u32,
}

impl SrvHeap {
    pub(crate) fn new(device: &ID3D12Device) -> Result<Self, EngineError> {
        let heap: ID3D12DescriptorHeap = unsafe {
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: SRV_HEAP_CAPACITY,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
        }
        .map_err(|error| init_error_from("CreateDescriptorHeap(CBV_SRV_UAV)", error))?;
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

    pub(crate) fn atlas_table(&self, generation: u8) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        debug_assert!(
            generation < ATLAS_GENERATIONS,
            "atlas generation out of range"
        );
        self.gpu_handle(u32::from(generation) * ATLAS_SRVS)
    }

    pub(crate) fn slot_table(&self, window_base: u32, slot: usize) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        self.gpu_handle(window_base + slot as u32 * SLOT_SRVS)
    }

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
        let instances_srv = buffer_srv(instance_count.max(1), 128);
        let clips_srv = buffer_srv(clip_count.max(1), 48);
        let first = window_base + slot as u32 * SLOT_SRVS;
        unsafe {
            device.CreateShaderResourceView(
                instances,
                Some(&instances_srv),
                self.cpu_handle(first),
            );
            device.CreateShaderResourceView(clips, Some(&clips_srv), self.cpu_handle(first + 1));
        }
    }

    pub(crate) fn write_atlas(
        &self,
        device: &ID3D12Device,
        generation: u8,
        kind: AtlasKind,
        texture: &ID3D12Resource,
        layers: u32,
    ) {
        debug_assert!(
            generation < ATLAS_GENERATIONS,
            "atlas generation out of range"
        );
        let srv = texture_srv(kind, layers);
        let index = u32::from(generation) * ATLAS_SRVS + kind.index();
        unsafe {
            device.CreateShaderResourceView(texture, Some(&srv), self.cpu_handle(index));
        }
    }

    fn cpu_handle(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let base = unsafe { self.heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + (self.descriptor_size * index) as usize,
        }
    }

    fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        let base = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart() };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + u64::from(self.descriptor_size) * u64::from(index),
        }
    }
}

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

    pub(crate) fn allocate(&mut self) -> Option<u32> {
        if let Some(index) = self.free.pop() {
            return Some(ATLAS_DESCRIPTORS + index * WINDOW_SRVS);
        }
        if self.next < MAX_WINDOWS {
            let index = self.next;
            self.next += 1;
            return Some(ATLAS_DESCRIPTORS + index * WINDOW_SRVS);
        }
        None
    }

    pub(crate) fn free(&mut self, base: u32) {
        debug_assert!(base >= ATLAS_DESCRIPTORS);
        debug_assert_eq!(
            (base - ATLAS_DESCRIPTORS) % WINDOW_SRVS,
            0,
            "window descriptor base must be aligned"
        );
        self.free.push((base - ATLAS_DESCRIPTORS) / WINDOW_SRVS);
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

fn texture_srv(kind: AtlasKind, layers: u32) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: match kind {
            AtlasKind::Glyph => DXGI_FORMAT_R8_UNORM,
            AtlasKind::Image => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            AtlasKind::ColorGlyph => DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
        },
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: 0,
                ArraySize: layers,
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
    let state = unsafe { device.CreateGraphicsPipelineState(&desc) }
        .map_err(|error| init_error_from("CreateGraphicsPipelineState", error));
    unsafe { ManuallyDrop::drop(&mut desc.pRootSignature) };
    state
}

pub(crate) struct GeometryEntry {
    vertex: ID3D12Resource,
    vertex_view: D3D12_VERTEX_BUFFER_VIEW,
    index: ID3D12Resource,
    index_view: D3D12_INDEX_BUFFER_VIEW,
    index_count: u32,
    topology: D3D_PRIMITIVE_TOPOLOGY,
    pipeline: GeometryPipeline,
    generation: u32,
    revision: u64,
    vertex_state: D3D12_RESOURCE_STATES,
    index_state: D3D12_RESOURCE_STATES,
    last_used_frame: u64,
    storage: [GpuResourceHandle; 2],
}

impl GeometryEntry {
    pub(crate) fn vertex(&self) -> D3D12_VERTEX_BUFFER_VIEW {
        self.vertex_view
    }

    pub(crate) fn index(&self) -> D3D12_INDEX_BUFFER_VIEW {
        self.index_view
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

    pub(crate) fn storage(&self) -> [GpuResourceHandle; 2] {
        self.storage
    }
}

const EVICT_AFTER_FRAMES: u64 = 180;

fn geometry_is_stale(frame: u64, last_used_frame: u64) -> bool {
    frame.wrapping_sub(last_used_frame) > EVICT_AFTER_FRAMES
}

pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,
    uploads: crate::render::geometry_upload::GeometryUploadTracker,
    frame: u64,
}

impl GeometryRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            uploads: crate::render::geometry_upload::GeometryUploadTracker::new(),
            frame: 0,
        }
    }

    pub(crate) fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

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

    pub(crate) fn release(&self, shared: &super::device::SharedGpu) {
        for entry in self.entries.iter().flatten() {
            for handle in entry.storage() {
                let _ = shared.release(handle);
            }
        }
    }

    pub(crate) fn get(&self, id: GeometryId) -> Option<&GeometryEntry> {
        self.entries
            .get(id.index() as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.generation == id.generation())
    }

    pub(crate) fn touch(&mut self, id: GeometryId) {
        let frame = self.frame;
        if let Some(entry) = self
            .entries
            .get_mut(id.index() as usize)
            .and_then(Option::as_mut)
            .filter(|entry| entry.generation == id.generation())
        {
            entry.last_used_frame = frame;
        }
    }

    pub(crate) fn prepare(
        &mut self,
        shared: &super::device::SharedGpu,
        list: &ID3D12GraphicsCommandList,
        staging: &mut super::compositor::GeometryUploadArena,
        geometry_table: &[crate::paint::instance::GeometryDrawGpu],
        slot: usize,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<usize, EngineError> {
        staging.begin(slot);
        let plans = self.uploads.plan(geometry_table);
        let mut uploaded = 0usize;
        for plan in &plans {
            uploaded += self.apply_plan(shared, list, staging, slot, plan, retired)?;
        }
        Ok(uploaded)
    }

    fn apply_plan(
        &mut self,
        shared: &super::device::SharedGpu,
        list: &ID3D12GraphicsCommandList,
        staging: &mut super::compositor::GeometryUploadArena,
        slot: usize,
        plan: &crate::render::geometry_upload::GeometryUploadPlan,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<usize, EngineError> {
        let snapshot = &plan.snapshot;
        debug_assert!(
            !plan.vertex.full || plan.vertex.pages.len() == snapshot.vertex_pages().len(),
            "full vertex plan must upload every page"
        );
        debug_assert!(
            !plan.index.full || plan.index.pages.len() == snapshot.index_pages().len(),
            "full index plan must upload every page"
        );
        if snapshot.index_count() == 0 || snapshot.vertex_pages().is_empty() {
            return Err(EngineError::GeometryRegister {
                detail: smol_str::format_smolstr!(
                    "geometry {}:{} has no CPU data (stale handle or released before upload)",
                    plan.id.index(),
                    plan.id.generation()
                ),
            });
        }
        let entry_slot = plan.id.index() as usize;
        if self.entries.len() <= entry_slot {
            self.entries.resize_with(entry_slot + 1, || None);
        }
        if self.entries[entry_slot]
            .as_ref()
            .is_some_and(|entry| entry.generation != plan.id.generation())
            && let Some(old) = self.entries[entry_slot].take()
        {
            retired.extend(old.storage());
        }
        let first_sight = self.entries[entry_slot].is_none();
        if first_sight {
            let vertex = create_geometry_buffer(shared, plan.vertex.capacity)?;
            let index = create_geometry_buffer(shared, plan.index.capacity)?;
            let vertex_handle = shared.allocate(vertex.clone());
            let index_handle = shared.allocate(index.clone());
            self.entries[entry_slot] = Some(GeometryEntry {
                vertex_view: vertex_view(&vertex, 0, snapshot.vertex_stride()),
                index_view: index_view(&index, 0, index_format(snapshot.index_type())),
                vertex,
                index,
                index_count: 0,
                topology: D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
                pipeline: GeometryPipeline::Ui,
                generation: plan.id.generation(),
                revision: 0,
                vertex_state: D3D12_RESOURCE_STATE_COPY_DEST,
                index_state: D3D12_RESOURCE_STATE_COPY_DEST,
                last_used_frame: self.frame,
                storage: [vertex_handle, index_handle],
            });
        }
        let entry = self.entries[entry_slot]
            .as_mut()
            .expect("刚刚建好的几何条目");
        if plan.vertex.realloc && !first_sight {
            let vertex = create_geometry_buffer(shared, plan.vertex.capacity)?;
            let handle = shared.allocate(vertex.clone());
            let old = core::mem::replace(&mut entry.vertex, vertex);
            let old_handle = core::mem::replace(&mut entry.storage[0], handle);
            entry.vertex_state = D3D12_RESOURCE_STATE_COPY_DEST;
            let _ = old;
            retired.push(old_handle);
        }
        if plan.index.realloc && !first_sight {
            let index = create_geometry_buffer(shared, plan.index.capacity)?;
            let handle = shared.allocate(index.clone());
            let old = core::mem::replace(&mut entry.index, index);
            let old_handle = core::mem::replace(&mut entry.storage[1], handle);
            entry.index_state = D3D12_RESOURCE_STATE_COPY_DEST;
            let _ = old;
            retired.push(old_handle);
        }
        let mut uploaded = 0usize;
        if !plan.vertex.pages.is_empty() {
            if entry.vertex_state != D3D12_RESOURCE_STATE_COPY_DEST {
                transition_buffer(
                    list,
                    &entry.vertex,
                    entry.vertex_state,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                );
                entry.vertex_state = D3D12_RESOURCE_STATE_COPY_DEST;
            }
            for page in &plan.vertex.pages {
                let offset =
                    crate::paint::geometry::GeometrySnapshot::page_offset(page.page as usize);
                let (staging_resource, staging_offset) =
                    staging.stage(shared, slot, &page.bytes, retired)?;
                unsafe {
                    list.CopyBufferRegion(
                        &entry.vertex,
                        u64::from(offset),
                        staging_resource,
                        u64::from(staging_offset),
                        u64::from(page.bytes.len() as u32),
                    );
                }
                uploaded += page.bytes.len();
            }
        }
        if !plan.index.pages.is_empty() {
            if entry.index_state != D3D12_RESOURCE_STATE_COPY_DEST {
                transition_buffer(
                    list,
                    &entry.index,
                    entry.index_state,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                );
                entry.index_state = D3D12_RESOURCE_STATE_COPY_DEST;
            }
            for page in &plan.index.pages {
                let offset =
                    crate::paint::geometry::GeometrySnapshot::page_offset(page.page as usize);
                let (staging_resource, staging_offset) =
                    staging.stage(shared, slot, &page.bytes, retired)?;
                unsafe {
                    list.CopyBufferRegion(
                        &entry.index,
                        u64::from(offset),
                        staging_resource,
                        u64::from(staging_offset),
                        u64::from(page.bytes.len() as u32),
                    );
                }
                uploaded += page.bytes.len();
            }
        }
        if entry.vertex_state != D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER {
            transition_buffer(
                list,
                &entry.vertex,
                entry.vertex_state,
                D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER,
            );
            entry.vertex_state = D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER;
        }
        if entry.index_state != D3D12_RESOURCE_STATE_INDEX_BUFFER {
            transition_buffer(
                list,
                &entry.index,
                entry.index_state,
                D3D12_RESOURCE_STATE_INDEX_BUFFER,
            );
            entry.index_state = D3D12_RESOURCE_STATE_INDEX_BUFFER;
        }
        entry.vertex_view = vertex_view(
            &entry.vertex,
            snapshot.vertex_len(),
            snapshot.vertex_stride(),
        );
        entry.index_view = index_view(
            &entry.index,
            snapshot.index_len(),
            index_format(snapshot.index_type()),
        );
        entry.index_count = snapshot.index_count();
        entry.topology = match snapshot.topology() {
            Topology::TriangleList => D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            Topology::TriangleStrip => D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        };
        entry.pipeline = snapshot.pipeline();
        entry.revision = snapshot.revision();
        entry.generation = plan.id.generation();
        entry.last_used_frame = self.frame;
        Ok(uploaded)
    }
}

fn create_geometry_buffer(
    shared: &super::device::SharedGpu,
    capacity: u32,
) -> Result<ID3D12Resource, EngineError> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: u64::from(capacity.max(4)),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_NONE,
        ..D3D12_RESOURCE_DESC::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let mut out: Option<ID3D12Resource> = None;
    unsafe {
        shared.raw().CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut out,
        )
    }
    .map_err(|error| init_error_from("CreateCommittedResource(geometry)", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 geometry buffer missing"),
    })
}

fn vertex_view(resource: &ID3D12Resource, len: u32, stride: u32) -> D3D12_VERTEX_BUFFER_VIEW {
    let location = unsafe { resource.GetGPUVirtualAddress() };
    D3D12_VERTEX_BUFFER_VIEW {
        BufferLocation: location,
        SizeInBytes: len,
        StrideInBytes: stride,
    }
}

fn index_view(resource: &ID3D12Resource, len: u32, format: DXGI_FORMAT) -> D3D12_INDEX_BUFFER_VIEW {
    let location = unsafe { resource.GetGPUVirtualAddress() };
    D3D12_INDEX_BUFFER_VIEW {
        BufferLocation: location,
        SizeInBytes: len,
        Format: format,
    }
}

fn index_format(kind: IndexType) -> DXGI_FORMAT {
    match kind {
        IndexType::U16 => DXGI_FORMAT_R16_UINT,
        IndexType::U32 => DXGI_FORMAT_R32_UINT,
    }
}

fn transition_buffer(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: core::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: core::mem::ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    unsafe { list.ResourceBarrier(core::slice::from_ref(&barrier)) };
    unsafe {
        let transition: &mut D3D12_RESOURCE_TRANSITION_BARRIER = &mut barrier.Anonymous.Transition;
        core::mem::ManuallyDrop::drop(&mut transition.pResource);
    }
}

#[cfg(test)]
mod tests {

    use super::{
        ATLAS_DESCRIPTORS, ATLAS_GENERATIONS, ATLAS_SRVS, DescriptorArena, Pipeline, SrvHeap,
        WINDOW_SRVS,
    };
    use crate::{
        math::px,
        paint::geometry::{GeometryPipeline, IndexType, Topology},
        render::windows::device::Device,
    };

    #[test]
    fn pipeline_builds_on_warp() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let device = Device::new_warp().expect("WARP device");
        let pipeline = Pipeline::new(device.raw()).expect("pipeline");
        let _ = pipeline.root_signature();
        let _ = pipeline.quad();
        let _ = pipeline.geometry(GeometryPipeline::Ui);
    }

    #[test]
    fn geometry_is_evicted_only_after_the_grace_window() {
        use super::{EVICT_AFTER_FRAMES, geometry_is_stale};

        assert!(!geometry_is_stale(1_000, 1_000));
        assert!(!geometry_is_stale(1_000 + EVICT_AFTER_FRAMES, 1_000));
        assert!(geometry_is_stale(1_001 + EVICT_AFTER_FRAMES, 1_000));
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
        assert_eq!(first, ATLAS_DESCRIPTORS, "窗口段从图集双代固定段之后开始");
        assert_eq!(second, ATLAS_DESCRIPTORS + WINDOW_SRVS);
        let descriptor_size = unsafe {
            device.raw().GetDescriptorHandleIncrementSize(
                windows::Win32::Graphics::Direct3D12::D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            )
        };
        let generation0 = heap.atlas_table(0);
        let generation1 = heap.atlas_table(1);
        assert_ne!(generation0.ptr, 0);
        assert_eq!(
            generation1.ptr - generation0.ptr,
            u64::from(descriptor_size) * u64::from(ATLAS_SRVS),
            "两代图集描述符组必须相隔 ATLAS_SRVS 个描述符（换代不覆盖在飞代）"
        );
        assert_ne!(heap.slot_table(first, 0).ptr, generation0.ptr);
        assert_ne!(heap.slot_table(first, 1).ptr, heap.slot_table(first, 0).ptr);
        assert_eq!(ATLAS_GENERATIONS, 2, "C3 的判据建立在恰好两代之上");

        arena.free(first);
        assert_eq!(arena.allocate(), Some(first));
    }

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
    fn geometry_registry_uploads_pages_incrementally() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        super::super::debug::enable_debug_layer();
        let shared = super::super::device::SharedGpu::new_warp().expect("WARP shared gpu");
        let mut stream = super::super::queue::CommandStream::new(&shared).expect("command stream");
        let mut staging =
            super::super::compositor::GeometryUploadArena::new(&shared).expect("staging arena");
        let mut registry = super::GeometryRegistry::new();
        let mut store = crate::paint::geometry_store::GeometryStore::new();
        let mut vertices = Vec::new();
        for point in [[0.0f32, 0.0f32], [32.0, 0.0], [0.0, 32.0]] {
            vertices.extend_from_slice(&point[0].to_ne_bytes());
            vertices.extend_from_slice(&point[1].to_ne_bytes());
            vertices.extend_from_slice(&(-1.0f32).to_ne_bytes());
        }
        let indices: Vec<u8> = [0u16, 1, 2]
            .iter()
            .flat_map(|index| index.to_ne_bytes())
            .collect();
        let id = store
            .insert(crate::paint::geometry::GeometryDesc {
                vertex_bytes: bytes::Bytes::from(vertices),
                index_bytes: bytes::Bytes::from(indices),
                vertex_stride: GeometryPipeline::Ui.vertex_stride(),
                index_type: IndexType::U16,
                topology: Topology::TriangleList,
                bounds: crate::math::LogicalRect::new(px(0.0), px(0.0), px(32.0), px(32.0)),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册几何");
        let mut retired = Vec::new();
        let draw = |snapshot: std::sync::Arc<crate::paint::geometry::GeometrySnapshot>| {
            crate::paint::instance::GeometryDrawGpu {
                id,
                range: 0..3,
                transform: crate::math::Transform::IDENTITY,
                color: [1.0; 4],
                snapshot,
            }
        };
        let snapshot = store.snapshot_arc(id).expect("快照");
        let full_pages_bytes = snapshot.vertex_len() as usize + snapshot.index_len() as usize;

        let first = {
            let slot = stream.acquire_slot();
            assert!(stream.wait_slot_fence(slot.wait_value).is_ok());
            stream.reset_slot(slot).expect("reset");
            registry.begin_frame();
            let list = stream.list(slot);
            let uploaded = registry
                .prepare(
                    &shared,
                    list,
                    &mut staging,
                    &[draw(std::sync::Arc::clone(&snapshot))],
                    slot.index,
                    &mut retired,
                )
                .expect("prepare");
            stream.submit(slot).expect("submit");
            let _ = stream.wait_slot_fence(slot.signal_value);
            uploaded
        };
        assert_eq!(first, full_pages_bytes, "首帧整份上传");
        let entry = registry.get(id).expect("条目已就绪");
        assert_eq!(entry.index_count(), 3);
        assert_eq!(entry.vertex().StrideInBytes, 12);
        assert_eq!(entry.storage().len(), 2);

        let second = {
            let slot = stream.acquire_slot();
            assert!(stream.wait_slot_fence(slot.wait_value).is_ok());
            stream.reset_slot(slot).expect("reset");
            registry.begin_frame();
            let list = stream.list(slot);
            let uploaded = registry
                .prepare(
                    &shared,
                    list,
                    &mut staging,
                    &[draw(std::sync::Arc::clone(&snapshot))],
                    slot.index,
                    &mut retired,
                )
                .expect("prepare");
            stream.submit(slot).expect("submit");
            let _ = stream.wait_slot_fence(slot.signal_value);
            uploaded
        };
        assert_eq!(second, 0, "同版本帧不得重复上传");

        store
            .update_vertices(id, 0..12, &[7u8; 12])
            .expect("range 更新");
        let updated = store.snapshot_arc(id).expect("新快照");
        let third = {
            let slot = stream.acquire_slot();
            assert!(stream.wait_slot_fence(slot.wait_value).is_ok());
            stream.reset_slot(slot).expect("reset");
            registry.begin_frame();
            let list = stream.list(slot);
            let uploaded = registry
                .prepare(
                    &shared,
                    list,
                    &mut staging,
                    &[draw(updated)],
                    slot.index,
                    &mut retired,
                )
                .expect("prepare");
            stream.submit(slot).expect("submit");
            let _ = stream.wait_slot_fence(slot.signal_value);
            uploaded
        };
        assert!(
            third > 0 && third <= crate::paint::geometry::GEOMETRY_PAGE_BYTES,
            "只上传被触碰的页：{third} B"
        );

        let retired_before = retired.len();
        for _ in 0..=super::EVICT_AFTER_FRAMES {
            registry.begin_frame();
        }
        registry.retain_recent(&mut retired);
        assert!(registry.get(id).is_none(), "长期未绘制的几何条目必须被驱逐");
        assert!(
            retired.len() > retired_before,
            "驱逐必须交还顶点/索引缓冲句柄（此前只测了谓词）"
        );

        let _ = stream.wait_idle();
        let _ = stream.release_completed();
        registry.release(&shared);
        staging.release(&shared);
        for handle in retired {
            let _ = shared.release(handle);
        }
        drop(stream);
    }
}

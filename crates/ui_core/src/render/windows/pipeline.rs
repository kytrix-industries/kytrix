//! 根签名 / quad PSO / geometry PSO / 描述符堆 / 静态几何表
//! （契约见 `render/shaders/contract.md`）。
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
//!   必须每窗口一份；窗口只写自己的描述符区间，**互不加锁**。
//!
//! 「不加锁」有规范依据，不是遗漏：D3D12 Resource Binding 功能规范 *Creating Descriptors*
//! 一节明确写着「**All methods for creating descriptors are free threaded**」
//! （<https://microsoft.github.io/DirectX-Specs/d3d/ResourceBinding.html>），各窗口写入的是
//! **不相交**的描述符区间，因此并发 `CreateShaderResourceView` 合法。Linux/Vulkan 那边必须
//! 加锁是因为 `vkUpdateDescriptorSets` 规范要求**外部同步** —— 两平台做法不同是各自 API 的要求，
//! 不是不对称缺陷。
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
            DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8_UNORM,
            DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32_UINT,
            DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
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
/// 每代图集描述符组：`[glyph, image]`。
const ATLAS_SRVS: u32 = 2;
/// 图集描述符组的代数（C3 双代：换代只写**未在用**的一代，旧一代在飞命令列表的引用保持有效）。
pub(crate) const ATLAS_GENERATIONS: u8 = 2;
/// 固定段描述符数（所有代）：`ATLAS_SRVS × ATLAS_GENERATIONS`。
const ATLAS_DESCRIPTORS: u32 = ATLAS_SRVS * ATLAS_GENERATIONS as u32;
/// 堆容量：固定段 + 340 个窗口（每描述符 32 B ⇒ 约 64 KiB，相对显存可忽略）。
const SRV_HEAP_CAPACITY: u32 = 2048;
/// 最多同时存在的窗口数（由堆容量决定；超出时创建渲染器返回 `AppInit`）。
pub(crate) const MAX_WINDOWS: u32 = (SRV_HEAP_CAPACITY - ATLAS_DESCRIPTORS) / WINDOW_SRVS;

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
        // geometry `Ui`：position float2 @0 + 覆盖率距离 float @8（解析 AA，R05-6）。
        let ui_layout = [
            input_element(c"POSITION", DXGI_FORMAT_R32G32_FLOAT, 0, 0),
            input_element(c"TEXCOORD", DXGI_FORMAT_R32_FLOAT, 0, 8),
        ];
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
/// 布局（描述符索引）：`[0, ATLAS_DESCRIPTORS)` 是图集的**双代**固定段（代 g 的 glyph @
/// `g × ATLAS_SRVS`、image @ `g × ATLAS_SRVS + 1`）；之后是每窗口段（`[instances, clips]` ×
/// in-flight 槽）。窗口段由 [`DescriptorArena`] 分配，窗口只写自己的区间。D3D12 每个 command
/// list 每种堆只能绑一张（`SetDescriptorHeaps` 文档），所以实例 / 裁剪与图集必须共用这一张堆 ——
/// 根签名的两张描述符表指向同一张堆的两个区间。
///
/// **双代的目的（C3）**：D3D12《Descriptor Heaps Overview》规定描述符在「提交执行中的命令列表
/// 可能引用它」期间不得被 CPU 改写。图集页换代只改代数里**没有在飞引用**的那一代（代际水位见
/// `SharedGpu::rebuild_atlas_pages`）；新帧按当前代绑定图集表（根参数 2 的基址），旧帧继续绑定
/// 旧代，两者互不覆盖。
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

    /// 某代图集表基址（root parameter 2；glyph @该代基址、image @该代基址+1）。
    pub(crate) fn atlas_table(&self, generation: u8) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        debug_assert!(
            generation < ATLAS_GENERATIONS,
            "atlas generation out of range"
        );
        self.gpu_handle(u32::from(generation) * ATLAS_SRVS)
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

    /// 写**指定代**里一个图集的 SRV（另一代与另一图集的描述符保持不变）。
    ///
    /// `layers` 是该图集 texture array 的**运行时**层数（字形 1、图片 = App 当前打开的图集页数）：
    /// SRV 的 `ArraySize` 必须与纹理的数组长度一致，否则 D3D12 创建视图失败。
    ///
    /// 调用方负责代际前置条件：只允许写「没有在飞命令列表引用」的那一代（C3；由
    /// `SharedGpu::rebuild_atlas_pages` 的代际水位判据保证）。
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
        // SAFETY: 设备 / 堆句柄由调用方持有，调用期间有效；描述符索引在固定段内。
        unsafe {
            device.CreateShaderResourceView(texture, Some(&srv), self.cpu_handle(index));
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
            return Some(ATLAS_DESCRIPTORS + index * WINDOW_SRVS);
        }
        if self.next < MAX_WINDOWS {
            let index = self.next;
            self.next += 1;
            return Some(ATLAS_DESCRIPTORS + index * WINDOW_SRVS);
        }
        None
    }

    /// 归还一个窗口段（`base` 是 [`SrvHeap::slot_table`] 用的段起始索引）。
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
        // 字形图集是线性覆盖率，**不能**声明 sRGB（P07 §2.9）。
        Format: match kind {
            AtlasKind::Glyph => DXGI_FORMAT_R8_UNORM,
            AtlasKind::Image => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        },
        // 图集是 texture array（字形 1 层、图片 = 当前打开的图集页数），层号是采样坐标。
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: 0,
                // 必须等于纹理的数组长度（`AtlasArraySpec::layers`），否则视图创建失败。
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

/// 一条已注册静态几何的 GPU 视图。
///
/// X5：顶点/索引缓冲在 **DEFAULT 堆**（设备本地），数据更新走 staging + `CopyBufferRegion`
/// 的**页级增量上传**（只传 `GeometrySnapshot` 里变化的 4 KiB 页）；缓冲增长时新建缓冲并
/// 整份重传（容量按 `GEOMETRY_BUFFER_GROWTH_ALIGN` 对齐，摊薄扩容频率）。
pub(crate) struct GeometryEntry {
    vertex: ID3D12Resource,
    vertex_view: D3D12_VERTEX_BUFFER_VIEW,
    index: ID3D12Resource,
    index_view: D3D12_INDEX_BUFFER_VIEW,
    index_count: u32,
    topology: D3D_PRIMITIVE_TOPOLOGY,
    pipeline: GeometryPipeline,
    /// 该条目对应的代际（槽位复用后必须重建缓冲，否则会画成新几何）。
    generation: u32,
    /// 已上传的几何快照版本。
    revision: u64,
    /// 两个缓冲当前的 D3D12 资源状态（DEFAULT 堆必须显式 barrier 才能 copy / 顶点读取）。
    vertex_state: D3D12_RESOURCE_STATES,
    index_state: D3D12_RESOURCE_STATES,
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

    /// 卸载时交还的句柄。
    pub(crate) fn storage(&self) -> [GpuResourceHandle; 2] {
        self.storage
    }
}

/// 连续多少帧未被绘制就回收该几何的 GPU 缓冲（180 帧 ≈ 60 Hz 下 3 秒）。
const EVICT_AFTER_FRAMES: u64 = 180;

/// 该几何是否已连续 `EVICT_AFTER_FRAMES` 帧以上未被绘制（`wrapping_sub` 容忍回绕）。
fn geometry_is_stale(frame: u64, last_used_frame: u64) -> bool {
    frame.wrapping_sub(last_used_frame) > EVICT_AFTER_FRAMES
}

/// `GeometryId → GPU 缓冲`（按需创建；版本变化按页增量上传；长期未绘制即驱逐）。
///
/// 缓冲由 GPU 线程在**帧开始前的 prepare 阶段**按本帧几何表创建/更新：编码器把几何快照附在
/// `GeometryDrawGpu` 上随帧过线，因此这里不需要额外的跨线程注册通道。
/// 槽位复用（`generation` 变化）会**丢弃并重建**该槽的缓冲，避免旧图元画成新几何。
///
/// 每窗口一份：几何缓冲只被本窗口的命令列表引用，退役句柄走本窗口的 retire 队列，因此不需要
/// 跨窗口共享（共享它们会引入「哪个窗口的 fence 先到达」的所有权问题，见 D-1）。
pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,
    /// 页级上传计划器（每窗口一份；渲染器重建时 `invalidate`）。
    uploads: crate::render::geometry_upload::GeometryUploadTracker,
    /// 单调帧号：每个内容帧录制前自增，`last_used_frame` 与驱逐都基于它。
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

    /// 取已就绪条目（消费者：录制期的 draw 循环；条目由 [`GeometryRegistry::prepare`] 建好）。
    pub(crate) fn get(&self, id: GeometryId) -> Option<&GeometryEntry> {
        self.entries
            .get(id.index() as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.generation == id.generation())
    }

    /// 标记条目本帧被绘制（`retain_recent` 的驱逐判据）。
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

    /// X5：帧开始前把本帧几何表的**新版本页**增量上传到 DEFAULT 缓冲
    /// （staging + `CopyBufferRegion`）。
    ///
    /// 返回本帧实际 staging 上传的字节数（HUD / 测试断言面）。
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

    /// 应用一个几何的上传计划：必要时重建缓冲，把变化页写进 staging 并记录 copy + barrier。
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
        // 契约自检：`full` 计划必须覆盖全部页（否则 GPU 会留下上一代的旧页数据）。
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
        // 槽位复用：旧代际的条目与缓冲一起废弃。
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
        // 单缓冲扩容：新建并整份重传（另一个缓冲保持按页上传）。
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
                // SAFETY: 目标区间在缓冲容量内（页来自快照的页对齐切分）；staging 区间由
                // bump 分配保证不重叠且在本帧内有效。
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
                // SAFETY: 同顶点路径。
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
        // copy 之后过渡回顶点/索引读取状态（下一帧的 copy 再切回 COPY_DEST）。
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

/// 建一块 DEFAULT 堆（设备本地）缓冲；初始状态 COMMON（copy 前再显式过渡）。
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
    // SAFETY: COM 调用：设备句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
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
    // SAFETY: 资源由 `GeometryRegistry` 持有，视图只在录制期使用。
    let location = unsafe { resource.GetGPUVirtualAddress() };
    D3D12_VERTEX_BUFFER_VIEW {
        BufferLocation: location,
        SizeInBytes: len,
        StrideInBytes: stride,
    }
}

fn index_view(resource: &ID3D12Resource, len: u32, format: DXGI_FORMAT) -> D3D12_INDEX_BUFFER_VIEW {
    // SAFETY: 同 `vertex_view`。
    let location = unsafe { resource.GetGPUVirtualAddress() };
    D3D12_INDEX_BUFFER_VIEW {
        BufferLocation: location,
        SizeInBytes: len,
        Format: format,
    }
}

/// 索引格式 → DXGI 格式。
fn index_format(kind: IndexType) -> DXGI_FORMAT {
    match kind {
        IndexType::U16 => DXGI_FORMAT_R16_UINT,
        IndexType::U32 => DXGI_FORMAT_R32_UINT,
    }
}

/// 一次 DEFAULT 缓冲的资源状态过渡（副本 → 顶点/索引读取）。
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
    // SAFETY: `barrier` 在本次调用期间独占；`from_ref` 不转移所有权。
    unsafe { list.ResourceBarrier(core::slice::from_ref(&barrier)) };
    // SAFETY: union 载荷是 `ManuallyDrop`，drop 不会释放 COM 引用；记录已提交，显式释放。
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
        paint::geometry::{GeometryPipeline, IndexType, Topology},
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

    /// C3：图集固定段有 `ATLAS_GENERATIONS` 代，每代 `ATLAS_SRVS` 个（glyph/image）；
    /// 两代基址必须落在不同描述符上（互换时不覆盖），窗口段从固定段之后开始。
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
        // SAFETY: 只读查询描述符增量；设备 / 堆由本测试持有。
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
        // 每代内 glyph @+0、image @+1 由 shader 的 t0/t1 绑定约定保证（根签名 range 是 2）。
        assert_ne!(heap.slot_table(first, 0).ptr, generation0.ptr);
        assert_ne!(heap.slot_table(first, 1).ptr, heap.slot_table(first, 0).ptr);
        assert_eq!(ATLAS_GENERATIONS, 2, "C3 的判据建立在恰好两代之上");

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

    /// X5：DEFAULT 缓冲 + staging 页级增量上传的账本：首帧整份、同版本零上传、range 更新只上传
    /// 变化页、长度增长时按对齐粒度扩容并整份重传。
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
        // Ui 顶点 = position(f32×2) + 覆盖率(f32)，12 B/顶点；一个三角形。
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
                bounds: crate::math::Rect::new(0.0, 0.0, 32.0, 32.0),
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

        // 帧 1：首见 ⇒ 整份上传。
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

        // 帧 2：同一版本 ⇒ 零上传（CPU 与 GPU 的页指针完全相同）。
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

        // 帧 3：改一个顶点 ⇒ 只上传被触碰的页。
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

        // RX-16：registry 级驱逐必须真的把顶点/索引句柄交回 retire 队列。
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

        // teardown：staging 句柄与 buffer 句柄都归还，retire 队列不泄漏。
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

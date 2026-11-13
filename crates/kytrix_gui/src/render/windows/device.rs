//! `ID3D12Device` 的创建、D8 采集、DRED 与资源句柄表（`design/phase-08-d3d12.md` §5.1）。
//!
//! 产品路径只建**硬件设备**；WARP 设备仅测试 / CI（`#[cfg(any(test, feature = "test-support"))]`），
//! 不进发布物的任何运行时分支（D8 / I10）。

use std::{ffi::c_void, path::PathBuf};

use smol_str::SmolStr;
use windows::{
    Win32::Graphics::{
        Direct3D::{
            D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_12_0,
            D3D_FEATURE_LEVEL_12_1, D3D_FEATURE_LEVEL_12_2,
        },
        Direct3D12::*,
        Dxgi::*,
    },
    core::{HRESULT, Interface},
};

use crate::{error::EngineError, render::retire::GpuResourceHandle};

/// D8 硬基线（错误文案与安装器共用的唯一来源；主设计 §2.2）。
pub(crate) const D8_REQUIRED: &str =
    "FL 12_0 / SM 6.2 / ResourceBindingTier >= 2 / FLIP_SEQUENTIAL / BufferCount >= 2";

/// 启动时一次性采集的能力快照（含诊断字段；`warp` 只用于诊断，不参与基线）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuCaps {
    /// `DXGI_ADAPTER_DESC1.Description`。
    pub adapter: SmolStr,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_vram_mib: u64,
    /// `D3D_FEATURE_LEVEL.0`。
    pub feature_level: i32,
    /// `D3D_SHADER_MODEL.0`（用 6_8 作为入参查询后的返回值）。
    pub shader_model: i32,
    pub resource_binding_tier: i32,
    pub resource_heap_tier: i32,
    pub tiled_resources_tier: i32,
    pub min_precision_support: i32,
    pub native_16bit_shader_ops: bool,
    pub root_signature_max: i32,
    pub max_sampler_descriptor_heap: u32,
    /// test / CI 的 WARP 设备标记（诊断用；产品路径恒 `false`）。
    pub warp: bool,
}

impl GpuCaps {
    /// 硬基线三项：FL >= 12_0、SM >= 6.2、BindingTier >= 2。
    pub(crate) fn baseline_ok(&self) -> bool {
        self.feature_level >= FEATURE_LEVEL_12_0
            && self.shader_model >= SHADER_MODEL_6_2
            && self.resource_binding_tier >= 2
    }

    /// `UnsupportedGpu.actual` 的稳定格式（只含三项基线值；测试与用户比对共用）。
    pub(crate) fn actual(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "FL {} / SM {} / BindingTier {}",
            fl_name(self.feature_level),
            sm_name(self.shader_model),
            self.resource_binding_tier
        )
    }

    /// 全字段诊断行（日志 / A-B 对照；不含时间戳）。
    pub(crate) fn describe(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "adapter={:?} vendor=0x{:04X} device=0x{:04X} vram={}MiB FL={} SM={} binding={} heap={} tiled={} min_precision={} native16={} root_sig={} max_sampler_heap={} warp={}",
            self.adapter,
            self.vendor_id,
            self.device_id,
            self.dedicated_vram_mib,
            fl_name(self.feature_level),
            sm_name(self.shader_model),
            self.resource_binding_tier,
            self.resource_heap_tier,
            self.tiled_resources_tier,
            self.min_precision_support,
            self.native_16bit_shader_ops,
            self.root_signature_max,
            self.max_sampler_descriptor_heap,
            self.warp
        )
    }

    /// 唯一校验入口：失败 → `EngineError::UnsupportedGpu`（列出实际值与最低要求，I10）。
    pub(crate) fn check(&self) -> Result<(), EngineError> {
        if self.baseline_ok() {
            Ok(())
        } else {
            Err(EngineError::UnsupportedGpu {
                os: "windows",
                required: D8_REQUIRED,
                actual: self.actual(),
            })
        }
    }
}

/// `D3D_FEATURE_LEVEL_12_0` 的原始值（不在共享层引 `windows` 类型）。
pub(crate) const FEATURE_LEVEL_12_0: i32 = 49_152;
/// `D3D_SHADER_MODEL_6_2` 的原始值。
pub(crate) const SHADER_MODEL_6_2: i32 = 98;

/// 把 `D3D_FEATURE_LEVEL.0` 映射成 `"12_0"` 之类的短名（唯一转换点）。
pub(crate) fn fl_name(level: i32) -> &'static str {
    match level {
        45_056 => "11_0",
        45_312 => "11_1",
        49_152 => "12_0",
        49_408 => "12_1",
        49_664 => "12_2",
        _ => "unknown",
    }
}

/// 把 `D3D_SHADER_MODEL.0` 映射成 `"6_2"` 之类的短名（唯一转换点）。
pub(crate) fn sm_name(shader_model: i32) -> &'static str {
    match shader_model {
        96 => "6_0",
        97 => "6_1",
        98 => "6_2",
        99 => "6_3",
        100 => "6_4",
        101 => "6_5",
        102 => "6_6",
        103 => "6_7",
        104 => "6_8",
        105 => "6_9",
        _ => "unknown",
    }
}

/// `GpuResourceHandle` 的 `kind`：COM 资源（P08 唯一产生的种类）。
const HANDLE_KIND_OBJECT: u8 = 1;

/// 把 HRESULT 变成 `AppInit` 的 `detail`（`{:#010X} {系统消息}`，可读）。
pub(crate) fn init_error(context: &str, hr: HRESULT) -> EngineError {
    EngineError::AppInit {
        detail: smol_str::format_smolstr!(
            "d3d12 {context}: {:#010X} {}",
            hr.0 as u32,
            hr.message()
        ),
    }
}

/// 把 `windows::core::Error` 变成 `AppInit`。
pub(crate) fn init_error_from(context: &str, error: windows::core::Error) -> EngineError {
    init_error(context, error.code())
}

/// 一个 `ID3D12Device` + 资源句柄表 + 丢失状态。
pub(crate) struct Device {
    raw: ID3D12Device,
    resources: ResourceTable,
    lost: Option<LostInfo>,
}

/// 设备丢失的粘性记录（`EngineError` 不能 `Clone`，因此存字段、按需重建）。
struct LostInfo {
    reason: SmolStr,
    log_path: PathBuf,
}

impl Device {
    /// 产品路径：第一个非软件 DXGI 适配器 → D3D12 设备 → 采 caps → 校验 D8。
    pub(crate) fn new() -> Result<Self, EngineError> {
        // SAFETY: COM 创建调用：设备 / 工厂句柄有效，描述与输出指针在调用期间存活，失败按 HRESULT 返回。
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .map_err(|error| init_error_from("CreateDXGIFactory2", error))?;
        let adapter = pick_hardware_adapter(&factory)?;
        Self::from_adapter(&adapter, false)
    }

    /// test / CI：WARP 设备（不编译进发布物；D8 的软件回退禁令由 `cfg` 保证）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_warp() -> Result<Self, EngineError> {
        // SAFETY: COM 创建调用：设备 / 工厂句柄有效，描述与输出指针在调用期间存活，失败按 HRESULT 返回。
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .map_err(|error| init_error_from("CreateDXGIFactory2", error))?;
        // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
        let adapter: IDXGIAdapter = unsafe { factory.EnumWarpAdapter() }
            .map_err(|error| init_error_from("EnumWarpAdapter", error))?;
        let adapter: IDXGIAdapter1 = adapter
            .cast()
            .map_err(|error| init_error_from("cast IDXGIAdapter1", error))?;
        Self::from_adapter(&adapter, true)
    }

    fn from_adapter(adapter: &IDXGIAdapter1, warp: bool) -> Result<Self, EngineError> {
        enable_dred();
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
        unsafe { D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, &mut device) }
            .map_err(|error| init_error_from("D3D12CreateDevice", error))?;
        let raw = device.ok_or_else(|| EngineError::AppInit {
            detail: SmolStr::new_static("d3d12 D3D12CreateDevice returned no device"),
        })?;

        // SAFETY: `unsafe fn` 契约由调用点保证：设备刚创建且适配器存活。
        let caps = unsafe { collect_caps(&raw, adapter, warp) };
        caps.check()?;
        debug_assert!(
            !caps.describe().is_empty(),
            "gpu description must not be empty"
        );
        // caps 的全部字段都是启动期一次性检测项；这里显式丢弃（P09 的日志层会重新采集并记录）。
        let _ = (caps, warp);

        Ok(Self {
            raw,
            resources: ResourceTable::new(),
            lost: None,
        })
    }

    pub(crate) fn raw(&self) -> &ID3D12Device {
        &self.raw
    }

    /// 建一个 RTV（**视图格式**决定是否做 sRGB 编码，DEVIATIONS F-80）。
    pub(crate) fn create_render_target_view(
        &self,
        resource: &ID3D12Resource,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) {
        let desc = D3D12_RENDER_TARGET_VIEW_DESC {
            Format: format,
            ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_RTV {
                    MipSlice: 0,
                    PlaneSlice: 0,
                },
            },
        };
        // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
        unsafe {
            self.raw
                .CreateRenderTargetView(resource, Some(&desc), handle)
        };
    }

    /// 粘性丢失错误（`Some` 之后不得再碰 D3D12）。
    pub(crate) fn lost_error(&self) -> Option<EngineError> {
        self.lost.as_ref().map(|lost| EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        })
    }

    /// 登记一个资源，返回可放进 `RetireQueue` 的句柄。
    pub(crate) fn allocate(&mut self, resource: ID3D12Resource) -> GpuResourceHandle {
        self.resources.insert(resource)
    }

    /// 释放句柄对应的资源；陈旧句柄返回 `false`。
    pub(crate) fn release(&mut self, handle: GpuResourceHandle) -> bool {
        self.resources.release(handle)
    }

    /// 任一 D3D12 调用失败后的分类：设备丢失 → 粘性 `DeviceLost`（并落 DRED）；否则 `None`
    /// （调用方保留自己的错误分类）。
    pub(crate) fn note_failure(&mut self) -> Option<EngineError> {
        if self.lost.is_some() {
            return self.lost_error();
        }
        // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
        let reason = match unsafe { self.raw.GetDeviceRemovedReason() } {
            Ok(()) => return None,
            Err(error) => error.code(),
        };
        // 只有真正的「移除」类错误才升级为 DeviceLost；其它（如 INVALID_CALL）保留原分类。
        const REMOVED: u32 = 0x887A_0005;
        const HUNG: u32 = 0x887A_0006;
        const RESET: u32 = 0x887A_0007;
        const DRIVER_INTERNAL: u32 = 0x887A_0020;
        if !matches!(reason.0 as u32, REMOVED | HUNG | RESET | DRIVER_INTERNAL) {
            return None;
        }
        let log_path = self.write_dred_log();
        let lost = LostInfo {
            reason: smol_str::format_smolstr!("{:#010X} {}", reason.0 as u32, reason.message()),
            log_path,
        };
        let error = EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        };
        self.lost = Some(lost);
        Some(error)
    }

    /// DRED 落盘：写 breadcrumb / page-fault 的查询结果（写文件失败不掩盖原始错误）。
    fn write_dred_log(&self) -> PathBuf {
        let path = self.dred_dir();
        let mut body = String::from("kytrix d3d12 device removed diagnostics\n");
        match self.raw.cast::<ID3D12DeviceRemovedExtendedData>() {
            Ok(dred) => {
                // SAFETY: 句柄与指针由 `self` 持有，调用期间有效；错误按 HRESULT / `Result` 处理。
                match unsafe { dred.GetAutoBreadcrumbsOutput() } {
                    Ok(output) => {
                        let _ = std::fmt::write(
                            &mut body,
                            format_args!(
                                "breadcrumbs: head_null={}\n",
                                output.pHeadAutoBreadcrumbNode.is_null()
                            ),
                        );
                    }
                    Err(error) => {
                        let _ = std::fmt::write(
                            &mut body,
                            format_args!("breadcrumbs: failed {:#010X}\n", error.code().0 as u32),
                        );
                    }
                }
                // SAFETY: 句柄与指针由 `self` 持有，调用期间有效；错误按 HRESULT / `Result` 处理。
                match unsafe { dred.GetPageFaultAllocationOutput() } {
                    Ok(output) => {
                        let _ = std::fmt::write(
                            &mut body,
                            format_args!("page_fault: va=0x{:X}\n", output.PageFaultVA),
                        );
                    }
                    Err(error) => {
                        let _ = std::fmt::write(
                            &mut body,
                            format_args!("page_fault: failed {:#010X}\n", error.code().0 as u32),
                        );
                    }
                }
            }
            Err(_) => body.push_str("DRED interface unavailable\n"),
        }
        let _ = std::fs::write(&path, body);
        path
    }

    fn dred_dir(&self) -> PathBuf {
        std::env::temp_dir()
            .join(smol_str::format_smolstr!("kytrix-dred-{}.txt", std::process::id()).as_str())
    }
}

/// 第一个非软件适配器。
fn pick_hardware_adapter(factory: &IDXGIFactory4) -> Result<IDXGIAdapter1, EngineError> {
    let mut index = 0u32;
    loop {
        // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
        match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => {
                // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
                let desc = unsafe { adapter.GetDesc1() }
                    .map_err(|error| init_error_from("GetDesc1", error))?;
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                    return Ok(adapter);
                }
            }
            Err(_) => {
                return Err(EngineError::UnsupportedGpu {
                    os: "windows",
                    required: D8_REQUIRED,
                    actual: SmolStr::new_static("no hardware DXGI adapter"),
                });
            }
        }
        index += 1;
    }
}

/// 在建设备**之前**打开 DRED（breadcrumbs + page fault）；接口不可用时静默跳过。
fn enable_dred() {
    let mut settings: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
    // SAFETY: 调试层接口是可选能力：本机缺 SDK 组件时返回错误并被跳过。
    if unsafe { D3D12GetDebugInterface(&mut settings) }.is_ok()
        && let Some(settings) = settings
    {
        // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
        unsafe {
            settings.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            settings.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
        }
    }
}

/// 采集 D8 检测项（顺序与主设计 §2.2 的 8 项表一致；每项失败都只填默认值）。
unsafe fn collect_caps(device: &ID3D12Device, adapter: &IDXGIAdapter1, warp: bool) -> GpuCaps {
    // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
    let desc = unsafe { adapter.GetDesc1() }.unwrap_or_default();
    let name_len = desc
        .Description
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(desc.Description.len());
    let adapter_name = SmolStr::new(String::from_utf16_lossy(&desc.Description[..name_len]));

    let levels = [
        D3D_FEATURE_LEVEL_12_2,
        D3D_FEATURE_LEVEL_12_1,
        D3D_FEATURE_LEVEL_12_0,
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
    ];
    let mut feature_levels = D3D12_FEATURE_DATA_FEATURE_LEVELS {
        NumFeatureLevels: levels.len() as u32,
        pFeatureLevelsRequested: levels.as_ptr(),
        MaxSupportedFeatureLevel: D3D_FEATURE_LEVEL_11_0,
    };
    check_feature(
        device,
        D3D12_FEATURE_FEATURE_LEVELS,
        &mut feature_levels,
        std::mem::size_of::<D3D12_FEATURE_DATA_FEATURE_LEVELS>(),
    );

    // F-73：入参必须是期望上限，否则永远得到填入值。
    let mut shader_model = D3D12_FEATURE_DATA_SHADER_MODEL {
        HighestShaderModel: D3D_SHADER_MODEL_6_8,
    };
    check_feature(
        device,
        D3D12_FEATURE_SHADER_MODEL,
        &mut shader_model,
        std::mem::size_of::<D3D12_FEATURE_DATA_SHADER_MODEL>(),
    );

    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    check_feature(
        device,
        D3D12_FEATURE_D3D12_OPTIONS,
        &mut options,
        std::mem::size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS>(),
    );

    let mut options4 = D3D12_FEATURE_DATA_D3D12_OPTIONS4::default();
    check_feature(
        device,
        D3D12_FEATURE_D3D12_OPTIONS4,
        &mut options4,
        std::mem::size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS4>(),
    );

    let mut root_signature = D3D12_FEATURE_DATA_ROOT_SIGNATURE {
        HighestVersion: D3D_ROOT_SIGNATURE_VERSION_1_0,
    };
    check_feature(
        device,
        D3D12_FEATURE_ROOT_SIGNATURE,
        &mut root_signature,
        std::mem::size_of::<D3D12_FEATURE_DATA_ROOT_SIGNATURE>(),
    );

    let mut options19 = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
    check_feature(
        device,
        D3D12_FEATURE_D3D12_OPTIONS19,
        &mut options19,
        std::mem::size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS19>(),
    );

    GpuCaps {
        adapter: adapter_name,
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        dedicated_vram_mib: (desc.DedicatedVideoMemory / (1024 * 1024)) as u64,
        feature_level: feature_levels.MaxSupportedFeatureLevel.0,
        shader_model: shader_model.HighestShaderModel.0,
        resource_binding_tier: options.ResourceBindingTier.0,
        resource_heap_tier: options.ResourceHeapTier.0,
        tiled_resources_tier: options.TiledResourcesTier.0,
        min_precision_support: options.MinPrecisionSupport.0,
        native_16bit_shader_ops: options4.Native16BitShaderOpsSupported.as_bool(),
        root_signature_max: root_signature.HighestVersion.0,
        max_sampler_descriptor_heap: options19.MaxSamplerDescriptorHeapSize,
        warp,
    }
}

/// 单个 `CheckFeatureSupport`（失败只留下默认值：这些字段是记录项，不是硬基线）。
fn check_feature<T>(device: &ID3D12Device, feature: D3D12_FEATURE, data: &mut T, size: usize) {
    // SAFETY: `data` 是按调用方给的 `size` 分配的栈上结构体，D3D12 只写入该大小。
    unsafe {
        let _ = device.CheckFeatureSupport(
            feature,
            std::ptr::from_mut(data).cast::<c_void>(),
            size as u32,
        );
    }
}

/// 资源句柄表（generation 拒绝陈旧句柄；P08 只有 `kind = 1`）。
struct ResourceTable {
    slots: Vec<Option<ID3D12Resource>>,
    generations: Vec<u32>,
    free: Vec<u32>,
}

impl ResourceTable {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            generations: Vec::new(),
            free: Vec::new(),
        }
    }

    fn insert(&mut self, resource: ID3D12Resource) -> GpuResourceHandle {
        if let Some(index) = self.free.pop() {
            let generation = self.generations[index as usize].wrapping_add(1);
            self.generations[index as usize] = generation;
            self.slots[index as usize] = Some(resource);
            GpuResourceHandle::new(HANDLE_KIND_OBJECT, generation, index)
        } else {
            let index = self.slots.len() as u32;
            self.slots.push(Some(resource));
            self.generations.push(0);
            GpuResourceHandle::new(HANDLE_KIND_OBJECT, 0, index)
        }
    }

    fn release(&mut self, handle: GpuResourceHandle) -> bool {
        if handle.kind() != HANDLE_KIND_OBJECT {
            debug_assert!(
                false,
                "unsupported GpuResourceHandle kind {}",
                handle.kind()
            );
            return false;
        }
        let index = handle.index() as usize;
        if self.generations.get(index).copied() != Some(handle.generation()) {
            return false;
        }
        if !matches!(self.slots.get(index), Some(Some(_))) {
            return false;
        }
        self.slots[index] = None;
        self.free.push(index as u32);
        true
    }
}

/// GPU 测试的进程级串行锁：多个 WARP 设备并行会互相拖垮（本机实测 device removed）。
#[cfg(test)]
pub(crate) fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use windows::{
        Win32::Graphics::{
            Direct3D12::*,
            Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC},
        },
        core::Interface,
    };

    use super::Device;
    use crate::error::EngineError;

    fn debug_texture(device: &Device) -> Result<ID3D12Resource, EngineError> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 256,
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
        // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
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
        .map_err(|error| super::init_error_from("CreateCommittedResource", error))?;
        out.ok_or_else(|| EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("d3d12 CreateCommittedResource returned nothing"),
        })
    }

    /// WARP：D8 基线（`new_warp` 内部 `caps.check()`）+ 设备健康。
    #[test]
    fn warp_device_is_created_and_healthy() {
        let mut device = Device::new_warp().expect("WARP device meets D8");
        assert!(device.lost_error().is_none());
        assert!(device.note_failure().is_none());
    }

    /// 设备真正被移除时：`note_failure` 升级为 `DeviceLost`（粘性）并落 DRED 日志。
    ///
    /// 用**硬件**设备：被移除的 WARP 设备会污染同进程后续的 WARP 创建（V-59）。
    #[test]
    fn removed_device_reports_device_lost_and_writes_a_log() {
        let _guard = super::gpu_test_lock();
        let Ok(mut device) = Device::new() else {
            eprintln!("skip: no hardware D3D12 device for the device-lost test");
            return;
        };
        assert!(device.note_failure().is_none(), "healthy device");
        {
            let device5: ID3D12Device5 = device.raw().cast().expect("ID3D12Device5");
            // SAFETY: 故意移除本测试独占的设备（Windows 会立即让后续调用返回 DEVICE_REMOVED）。
            unsafe { device5.RemoveDevice() };
        }
        let error = device
            .note_failure()
            .expect("removed device must be reported");
        match &error {
            EngineError::DeviceLost { reason, log_path } => {
                assert!(reason.to_lowercase().contains("0x887a0005"), "{reason}");
                let path = log_path.display().to_string().to_lowercase();
                assert!(path.ends_with(".txt"), "{path}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        // 粘性：同一设备不再重复写日志，但状态保持「已丢失」。
        assert!(device.note_failure().is_some());
        assert!(device.lost_error().is_some());
    }

    #[test]
    fn resource_table_reuses_slots_with_a_new_generation() {
        let mut device = Device::new_warp().expect("WARP device");
        let texture = debug_texture(&device).expect("debug texture");
        let handle = device.allocate(texture);
        assert!(device.release(handle));
        assert!(!device.release(handle), "double release must be rejected");

        let replacement = debug_texture(&device).expect("debug texture");
        let reused = device.allocate(replacement);
        assert_eq!(reused.index(), handle.index(), "slot is reused");
        assert_ne!(reused.generation(), handle.generation());
        assert!(!device.release(handle), "stale generation is rejected");
        assert!(device.release(reused));
    }
}

#[cfg(test)]
mod caps_tests {
    use smol_str::SmolStr;

    use super::{D8_REQUIRED, GpuCaps, fl_name, sm_name};
    use crate::error::EngineError;

    fn caps() -> GpuCaps {
        GpuCaps {
            adapter: SmolStr::new_static("Test Adapter"),
            vendor_id: 0x10DE,
            device_id: 0x2C02,
            dedicated_vram_mib: 15977,
            feature_level: 49_664,
            shader_model: 104,
            resource_binding_tier: 3,
            resource_heap_tier: 2,
            tiled_resources_tier: 4,
            min_precision_support: 2,
            native_16bit_shader_ops: true,
            root_signature_max: 1,
            max_sampler_descriptor_heap: 4080,
            warp: false,
        }
    }

    #[test]
    fn baseline_accepts_d8_capable_device() {
        assert!(caps().baseline_ok());
        assert!(caps().check().is_ok());
    }

    #[test]
    fn baseline_rejects_each_hard_requirement() {
        let mut below_fl = caps();
        below_fl.feature_level = 45_056;
        assert!(!below_fl.baseline_ok());

        let mut below_sm = caps();
        below_sm.shader_model = 96;
        assert!(!below_sm.baseline_ok());

        let mut below_tier = caps();
        below_tier.resource_binding_tier = 1;
        assert!(!below_tier.baseline_ok());
    }

    #[test]
    fn check_reports_required_and_actual() {
        let mut below = caps();
        below.feature_level = 45_056;
        below.shader_model = 96;
        below.resource_binding_tier = 1;

        let error = below.check().expect_err("baseline must fail");
        match &error {
            EngineError::UnsupportedGpu {
                os,
                required,
                actual,
            } => {
                assert_eq!(*os, "windows");
                assert_eq!(*required, D8_REQUIRED);
                assert_eq!(actual.as_str(), "FL 11_0 / SM 6_0 / BindingTier 1");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        let text = error.to_string();
        assert!(text.contains("FL 11_0 / SM 6_0 / BindingTier 1"), "{text}");
        assert!(text.contains("release notes"), "{text}");
        assert!(!text.to_lowercase().contains("falling back"), "{text}");
        assert!(
            !text.to_lowercase().contains("software rendering"),
            "{text}"
        );
    }

    #[test]
    fn describe_lists_every_diagnostic_field() {
        let text = caps().describe();
        for needle in [
            "adapter=\"Test Adapter\"",
            "vendor=0x10DE",
            "device=0x2C02",
            "vram=15977MiB",
            "FL=12_2",
            "SM=6_8",
            "binding=3",
            "heap=2",
            "tiled=4",
            "min_precision=2",
            "native16=true",
            "root_sig=1",
            "max_sampler_heap=4080",
            "warp=false",
        ] {
            assert!(text.contains(needle), "missing {needle} in {text}");
        }
    }

    #[test]
    fn level_names_cover_the_d8_range() {
        assert_eq!(fl_name(45_056), "11_0");
        assert_eq!(fl_name(45_312), "11_1");
        assert_eq!(fl_name(49_152), "12_0");
        assert_eq!(fl_name(49_408), "12_1");
        assert_eq!(fl_name(49_664), "12_2");
        assert_eq!(fl_name(0), "unknown");
        assert_eq!(sm_name(96), "6_0");
        assert_eq!(sm_name(98), "6_2");
        assert_eq!(sm_name(102), "6_6");
        assert_eq!(sm_name(104), "6_8");
        assert_eq!(sm_name(0), "unknown");
    }
}

//! 进程级共享 GPU（`SharedGpu`）+ `ID3D12Device` 的创建、D8 采集、DRED、调试层、恢复路径与
//! 资源句柄表。
//!
//! 多窗口的正确形状（与 Chrome / Qt RHI 一致）：进程一个 `ID3D12Device`、一条 direct queue、
//! 一套根签名 / PSO、两份共享图集纹理；每个窗口只保留 swapchain / DirectComposition 对象 /
//! persist 目标 / fence 与帧槽 / 命令分配器与命令列表。
//!
//! 共享状态全部有明确的同步策略：
//!
//! - **设备句柄表 / 设备丢失状态**（[`Device`]）在 `Mutex` 里：`allocate` / `release` /
//!   `note_failure` 都是短临界区；只读的 `Create*` 调用走 `SharedGpu::raw` 的独立 COM 句柄，
//!   不需要锁。
//! - **根签名 / PSO** 创建后只读。
//! - **图集纹理由 `RwLock` 保护**：窗口的 GPU 线程在**读锁**内录制并提交一帧（页在录制期间不会
//!   变），页重建走**写锁**。写锁期间不可能出现新的提交（录制与提交都在读锁内），因此页换代
//!   只需要等各窗口登记的**提交水位**（[`super::queue::StreamWatermark`]），不需要整条队列
//!   idle（R08-6）。
//! - **设备恢复**（R08-4）：进程级实例不再是 `OnceLock`，而是可替换的注册表（[`GpuCell`]）；
//!   设备真的被移除后，[`SharedGpu::recover`] 重建 device / queue / PSO / SRV 堆，并按
//!   [`super::atlas::SharedAtlas::shadow_snapshot`] 的 CPU 影子把图集内容原样重传。
//!
//! 产品路径只建**硬件设备**；WARP 设备仅测试 / CI（`#[cfg(any(test, feature = "test-support"))]`），
//! 不进发布物的任何运行时分支（D8 / I10）。测试没有真 GPU 时用的是另一个进程级 WARP 实例
//! （[`SharedGpu::process_warp`]）：与产品路径同构，但设备来源不同。

use std::{
    collections::VecDeque,
    ffi::c_void,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard, Weak,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use smol_str::SmolStr;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Graphics::{
            Direct3D::{
                D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_12_0,
                D3D_FEATURE_LEVEL_12_1, D3D_FEATURE_LEVEL_12_2,
            },
            Direct3D12::*,
            Dxgi::{
                Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC},
                *,
            },
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
    core::{HRESULT, Interface},
};

use super::{
    atlas::{SharedAtlas, row_pitch_for},
    debug,
    hresult::{is_device_removal, removal_reason_label},
    queue::{FENCE_TIMEOUT_MS, StreamWatermark},
};
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        atlas::{AtlasRebuilt, texel_bytes},
        retire::GpuResourceHandle,
        windows::pipeline::{DescriptorArena, MAX_WINDOWS, Pipeline, SrvHeap},
    },
};

/// D8 硬基线（错误文案与安装器共用的唯一来源；主设计 §2.2）。
///
/// `cast_srgb` 是本后端**唯一**依赖的非通用能力（R08-12）：持久目标与 back buffer 都是
/// `B8G8R8A8_UNORM` 资源 + `_UNORM_SRGB` 视图，靠 RS2+ 的 relaxed format casting 成立。
/// 不支持它就无法在本设计下正确编码 sRGB（改成真 sRGB 资源会让到 UNORM back buffer 的
/// `CopyTextureRegion` 失败），因此显式失败而不是静默输出偏暗的像素。
pub(crate) const D8_REQUIRED: &str = "FL 12_0 / SM 6.2 / ResourceBindingTier >= 2 / CastingFullyTypedFormat / FLIP_SEQUENTIAL / BufferCount >= 2";

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
    /// `D3D12_FEATURE_DATA_D3D12_OPTIONS3::CastingFullyTypedFormatSupported`（R08-12）。
    pub casting_fully_typed: bool,
    /// 调试层是否可用（`debug_assertions` 且本机装了「图形工具」；release 恒 `false`）。
    pub debug_layer: bool,
    /// test / CI 的 WARP 设备标记（诊断用；产品路径恒 `false`）。
    pub warp: bool,
}

impl GpuCaps {
    /// 硬基线四项：FL >= 12_0、SM >= 6.2、BindingTier >= 2、支持 relaxed format casting。
    pub(crate) fn baseline_ok(&self) -> bool {
        self.feature_level >= FEATURE_LEVEL_12_0
            && self.shader_model >= SHADER_MODEL_6_2
            && self.resource_binding_tier >= 2
            && self.casting_fully_typed
    }

    /// `UnsupportedGpu.actual` 的稳定格式（只含四项基线值；测试与用户比对共用）。
    pub(crate) fn actual(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "FL {} / SM {} / BindingTier {} / CastSrgb {}",
            fl_name(self.feature_level),
            sm_name(self.shader_model),
            self.resource_binding_tier,
            self.casting_fully_typed
        )
    }

    /// 全字段诊断行（日志 / A-B 对照；不含时间戳）。
    pub(crate) fn describe(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "adapter={:?} vendor=0x{:04X} device=0x{:04X} vram={}MiB FL={} SM={} binding={} heap={} tiled={} min_precision={} native16={} root_sig={} max_sampler_heap={} cast_srgb={} debug_layer={} warp={}",
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
            self.casting_fully_typed,
            self.debug_layer,
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

/// 一个 `ID3D12Device` + 资源句柄表 + 丢失状态 + 能力快照。
///
/// 产品路径里它是 `SharedGpu` 的私有字段（由互斥锁保护）；测试可以直接构造一个**独立**设备，
/// 用于「设备被移除」这类会污染进程级共享设备的路径。
pub(crate) struct Device {
    raw: ID3D12Device,
    resources: ResourceTable,
    lost: Option<LostInfo>,
    caps: GpuCaps,
}

/// 设备丢失的粘性记录（`EngineError` 不能 `Clone`，因此存字段、按需重建）。
struct LostInfo {
    reason: SmolStr,
    log_path: PathBuf,
}

impl Device {
    /// 产品路径：第一个**高性能**非软件 DXGI 适配器 → D3D12 设备 → 采 caps → 校验 D8。
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
        // 调试层**必须**在 `D3D12CreateDevice` 之前打开（R08-5），否则层不会挂到设备上。
        debug::enable_debug_layer();
        enable_dred();
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
        unsafe { D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, &mut device) }
            .map_err(|error| init_error_from("D3D12CreateDevice", error))?;
        let raw = device.ok_or_else(|| EngineError::AppInit {
            detail: SmolStr::new_static("d3d12 D3D12CreateDevice returned no device"),
        })?;
        // 设备创建之后配 InfoQueue：只放行 WARNING 及更严重的消息（每帧 INFO 会刷屏）。
        debug::configure_info_queue(&raw);

        // SAFETY: `unsafe fn` 契约由调用点保证：设备刚创建且适配器存活。
        let caps = unsafe { collect_caps(&raw, adapter, warp) };
        caps.check()?;
        debug_assert!(
            !caps.describe().is_empty(),
            "gpu description must not be empty"
        );

        Ok(Self {
            raw,
            resources: ResourceTable::new(),
            lost: None,
            caps,
        })
    }

    pub(crate) fn raw(&self) -> &ID3D12Device {
        &self.raw
    }

    /// 启动时采集的能力快照（`SharedGpu` 在创建时记录一行诊断）。
    pub(crate) fn caps(&self) -> &GpuCaps {
        &self.caps
    }

    /// 测试：当前仍有 COM 引用的资源数（teardown 泄漏回归面）。
    #[cfg(test)]
    pub(crate) fn live_resources(&self) -> usize {
        self.resources
            .slots
            .iter()
            .filter(|slot| slot.is_some())
            .count()
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
    ///
    /// `GetDeviceRemovedReason()` 是**唯一**权威判据（R08-3）：`S_OK` 时永不返回 `DeviceLost`。
    pub(crate) fn note_failure(&mut self) -> Option<EngineError> {
        if self.lost.is_some() {
            return self.lost_error();
        }
        // SAFETY: 设备句柄由本结构持有，调用期间有效；查询无副作用。
        let reason = match unsafe { self.raw.GetDeviceRemovedReason() } {
            Ok(()) => return None,
            Err(error) => error.code(),
        };
        if !is_device_removal(reason) {
            return None;
        }
        let log_path = self.write_dred_log();
        let lost = LostInfo {
            reason: smol_str::format_smolstr!(
                "{:#010X} {} ({})",
                reason.0 as u32,
                reason.message(),
                removal_reason_label(reason)
            ),
            log_path,
        };
        let error = EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        };
        self.lost = Some(lost);
        Some(error)
    }

    /// DRED + 调试层落盘：写 breadcrumb / page-fault 的查询结果与 InfoQueue 计数
    /// （写文件失败不掩盖原始错误）。
    fn write_dred_log(&self) -> PathBuf {
        let path = self.dred_dir();
        let mut body = String::from("kytrix d3d12 device removed diagnostics\n");
        // SAFETY: 设备句柄由本结构持有，调用期间有效；查询无副作用。
        if let Err(error) = unsafe { self.raw.GetDeviceRemovedReason() } {
            let _ = std::fmt::write(
                &mut body,
                format_args!(
                    "GetDeviceRemovedReason: {:#010X} {} ({})\n",
                    error.code().0 as u32,
                    error.code().message(),
                    removal_reason_label(error.code())
                ),
            );
        }
        let messages = debug::drain_messages(&self.raw);
        let _ = std::fmt::write(
            &mut body,
            format_args!(
                "debug layer messages: warnings={} errors={} corruption={}\n",
                messages.warning, messages.error, messages.corruption
            ),
        );
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
            Err(_) => body.push_str(
                "DRED interface unavailable (requires the Graphics Tools optional feature)\n",
            ),
        }
        let _ = std::fs::write(&path, body);
        path
    }

    fn dred_dir(&self) -> PathBuf {
        std::env::temp_dir()
            .join(smol_str::format_smolstr!("kytrix-dred-{}.txt", std::process::id()).as_str())
    }
}

/// 进程级共享 GPU：设备 + 一条 direct queue + 根签名 / PSO + 共享图集纹理 / SRV 堆。
///
/// 构造函数都是幂等的**进程级单例**（见 [`SharedGpu::process`]）；测试可以直接构造独立实例
/// （[`SharedGpu::new_warp`]）以隔离图集状态。
pub(crate) struct SharedGpu {
    /// 设备的独立 COM 引用：只读 `Create*` 调用不需要拿锁。
    raw: ID3D12Device,
    /// 资源句柄表 + 粘性丢失状态（写少、临界区短）。
    device: Mutex<Device>,
    /// 进程唯一的一条 direct queue（D3D12 的 command queue 是 free-threaded 的）。
    queue: ID3D12CommandQueue,
    pipeline: Pipeline,
    /// 进程唯一的 shader-visible SRV 堆（图集固定段 + 每窗口段）。
    srv_heap: SrvHeap,
    /// 共享 SRV 堆的窗口段分配器（窗口创建 / 销毁时分配 / 归还）。
    srv_arena: Mutex<DescriptorArena>,
    /// 共享图集纹理：读锁 = 录制并提交一帧；写锁 = 页重建（见模块文档）。
    atlas: RwLock<SharedAtlas>,
    /// 当前共享图集页尺寸的无锁快照（`[glyph, image]`；上传入队时打「入队页」标记用）。
    atlas_pages: [AtomicU32; 2],
    /// 各窗口提交流登记的**提交水位**（R08-6）：页换代的最小等待集。
    watermarks: Mutex<Vec<Weak<StreamWatermark>>>,
    /// 页换代慢路径的延迟销毁表（等不到某个窗口的水位时，旧纹理先留在这里）。
    deferred_atlas: Mutex<Vec<ID3D12Resource>>,
}

// SAFETY: D3D12 的 device / queue / fence / descriptor heap 都是 free-threaded 的 COM 对象，
// 允许跨线程调用；本结构把**可变**状态全部收进了同步原语：
// - `device`（资源句柄表 + 丢失状态）在 `Mutex` 里；
// - 图集纹理与共享图集 SRV 堆由 `atlas` 的 `RwLock` 保护：描述符只会在写锁内、且等到全部
//   窗口的提交水位之后被重写，读锁持有者在录制 / 提交期间不会看到页变化；
// - `pipeline`（根签名 / PSO）创建后只读；`watermarks` 是 `Weak` 列表。
unsafe impl Send for SharedGpu {}
unsafe impl Sync for SharedGpu {}

impl SharedGpu {
    /// 产品路径的进程级共享 GPU（硬件设备；第一个窗口创建时初始化）。
    ///
    /// 有健康实例就复用；实例已丢失或还没建时新建一个（初始化失败不缓存：`EngineError` 不是
    /// `Clone`，且失败后的重试成本远低于把进程钉在错误状态上）。
    pub(crate) fn process() -> Result<Arc<Self>, EngineError> {
        let mut cell = gpu_cell();
        if let Some(gpu) = cell.current.as_ref().and_then(Weak::upgrade)
            && gpu.lost_error().is_none()
        {
            return Ok(gpu);
        }
        let gpu = Arc::new(Self::create(Device::new()?)?);
        cell.current = Some(Arc::downgrade(&gpu));
        cell.generation += 1;
        Ok(gpu)
    }

    /// 测试 / `test-support` 的进程级共享 GPU（WARP 设备；与产品路径同构）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn process_warp() -> Result<Arc<Self>, EngineError> {
        static GPU: OnceLock<Arc<SharedGpu>> = OnceLock::new();
        static INIT: Mutex<()> = Mutex::new(());
        shared_instance(&GPU, &INIT, || Self::create(Device::new_warp()?))
    }

    /// 测试用：独立实例（不进入进程级单例；用于隔离图集页状态 / 验证并发）。
    #[cfg(test)]
    pub(crate) fn new_warp() -> Result<Arc<Self>, EngineError> {
        Ok(Arc::new(Self::create(Device::new_warp()?)?))
    }

    /// **设备丢失后的进程内恢复**（R08-4）。
    ///
    /// 三条规则：
    /// 1. 别的窗口已经重建过（注册表里的当前实例不是 `stale` 且健康）⇒ 直接复用它，避免每个
    ///    窗口各建一套 device / PSO / 图集；
    /// 2. 恢复尝试限次（[`MAX_RECOVERIES_PER_WINDOW`] / [`RECOVERY_WINDOW`]），避免「每次出帧都
    ///    重建一次设备」把 CPU 打满；
    /// 3. 新设备的图集按 `stale` 的 CPU 影子页重传（否则恢复后的窗口没有字形/图片）。
    pub(crate) fn recover(stale: &Arc<Self>) -> Result<Arc<Self>, EngineError> {
        let mut cell = gpu_cell();
        if let Some(current) = cell.current.as_ref().and_then(Weak::upgrade)
            && !Arc::ptr_eq(&current, stale)
            && current.lost_error().is_none()
        {
            return Ok(current);
        }
        let now = Instant::now();
        cell.recoveries
            .retain(|at| now.duration_since(*at) < RECOVERY_WINDOW);
        if cell.recoveries.len() >= MAX_RECOVERIES_PER_WINDOW {
            return Err(EngineError::DeviceLost {
                reason: smol_str::format_smolstr!(
                    "device recovery limit reached ({} attempts within {:?})",
                    cell.recoveries.len(),
                    RECOVERY_WINDOW
                ),
                log_path: PathBuf::new(),
            });
        }
        cell.recoveries.push_back(now);
        let snapshot = stale.atlas().shadow_snapshot();
        let gpu = Arc::new(Self::create(Device::new()?)?);
        gpu.restore_atlas(&snapshot)?;
        cell.current = Some(Arc::downgrade(&gpu));
        cell.generation += 1;
        Ok(gpu)
    }

    fn create(device: Device) -> Result<Self, EngineError> {
        // SAFETY: 设备刚创建且存活；队列描述是值参数。
        let queue: ID3D12CommandQueue = unsafe {
            device.raw().CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                Priority: 0,
                Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
                NodeMask: 0,
            })
        }
        .map_err(|error| init_error_from("CreateCommandQueue", error))?;
        let pipeline = Pipeline::new(device.raw())?;
        let atlas = SharedAtlas::new(device.raw())?;
        let atlas_pages = [
            AtomicU32::new(atlas.page(AtlasKind::Glyph)),
            AtomicU32::new(atlas.page(AtlasKind::Image)),
        ];
        let srv_heap = SrvHeap::new(device.raw())?;
        srv_heap.write_atlas(
            device.raw(),
            AtlasKind::Glyph,
            atlas.texture(AtlasKind::Glyph),
        );
        srv_heap.write_atlas(
            device.raw(),
            AtlasKind::Image,
            atlas.texture(AtlasKind::Image),
        );
        // 能力快照是进程级共享事实：启动时记录一行（诊断 / A-B 对照；P3-1 的 review 建议）。
        eprintln!("kytrix: d3d12 {}", device.caps().describe());
        let raw = device.raw().clone();
        Ok(Self {
            raw,
            device: Mutex::new(device),
            queue,
            pipeline,
            srv_heap,
            srv_arena: Mutex::new(DescriptorArena::new()),
            atlas: RwLock::new(atlas),
            atlas_pages,
            watermarks: Mutex::new(Vec::new()),
            deferred_atlas: Mutex::new(Vec::new()),
        })
    }

    /// 只读设备句柄（`Create*` / `GetDeviceRemovedReason` 等无状态查询用）。
    pub(crate) fn raw(&self) -> &ID3D12Device {
        &self.raw
    }

    /// 进程唯一的 direct queue（各窗口的 `CommandStream` 向它提交）。
    pub(crate) fn queue(&self) -> &ID3D12CommandQueue {
        &self.queue
    }

    /// 共享根签名 / PSO。
    pub(crate) fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// 进程唯一的 shader-visible SRV 堆（图集固定段 + 每窗口段）。
    pub(crate) fn srv_heap(&self) -> &SrvHeap {
        &self.srv_heap
    }

    /// 给一个窗口分配共享 SRV 堆里的窗口段（`Drop` 时归还）。
    pub(crate) fn window_descriptors(self: &Arc<Self>) -> Result<DescriptorLease, EngineError> {
        let base = self
            .srv_arena
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .allocate()
            .ok_or_else(|| EngineError::AppInit {
                detail: smol_str::format_smolstr!(
                    "srv descriptor heap exhausted (window segments: {})",
                    MAX_WINDOWS
                ),
            })?;
        Ok(DescriptorLease {
            gpu: Arc::clone(self),
            base,
        })
    }

    fn release_window_descriptors(&self, base: u32) {
        self.srv_arena
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .free(base);
    }

    /// 图集读锁：持有期间页不会变（录制 + 提交一帧的整个区间）。
    pub(crate) fn atlas(&self) -> RwLockReadGuard<'_, SharedAtlas> {
        self.atlas.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// 共享图集当前页尺寸的无锁快照（上传入队时记录「入队页」，见 `AtlasUploads`）。
    pub(crate) fn atlas_page(&self, kind: AtlasKind) -> u32 {
        self.atlas_pages[kind.index() as usize].load(Ordering::Acquire)
    }

    /// 登记一个提交流的水位（`CommandStream::new`；条目只持 `Weak`）。
    pub(crate) fn register_watermark(&self, fence: ID3D12Fence) -> Arc<StreamWatermark> {
        let watermark = Arc::new(StreamWatermark::new(fence));
        self.watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&watermark));
        watermark
    }

    /// 注销水位（`CommandStream::drop`）：顺手清掉已经失效的 `Weak`。
    pub(crate) fn unregister_watermark(&self, watermark: &Arc<StreamWatermark>) {
        let mut watermarks = self
            .watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        watermarks.retain(|entry| {
            entry.upgrade().is_some_and(|registered| {
                !std::ptr::eq(Arc::as_ptr(&registered), Arc::as_ptr(watermark))
            })
        });
    }

    /// 各窗口的当前水位（测试 / 诊断；顺序不保证）。
    #[cfg(test)]
    pub(crate) fn watermark_values(&self) -> Vec<u64> {
        self.watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .map(|watermark| watermark.value())
            .collect()
    }

    /// 等全部窗口的提交水位到达（R08-6：页换代的最小等待集）。
    ///
    /// 返回 `false` 表示「设备健康但**有**窗口的水位在 [`FENCE_TIMEOUT_MS`] 内没来」——
    /// 这不是错误（R08-3），只是慢路径：页换代照做，旧纹理先进延迟销毁表，下一次成功同步再释放。
    fn wait_watermarks(&self) -> Result<bool, EngineError> {
        let pending: Vec<(ID3D12Fence, u64)> = self
            .watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .map(|watermark| (watermark.fence(), watermark.value()))
            .filter(|(_, value)| *value > 0)
            .collect();
        if pending.is_empty() {
            return Ok(true);
        }
        let deadline = Instant::now() + Duration::from_millis(u64::from(FENCE_TIMEOUT_MS));
        let mut synced = true;
        for (fence, value) in pending {
            // SAFETY: fence 句柄由对应窗口的提交流持有（它是 `Weak` 的强引用方，页换代期间
            // 提交流一定还活着：调用方是窗口自己的 GPU 线程或它的锁持有者）。
            if unsafe { fence.GetCompletedValue() } >= value {
                continue;
            }
            // SAFETY: 无名称 auto-reset 事件，仅本次等待使用；句柄在返回前关闭。
            let event: HANDLE = unsafe { CreateEventW(None, false, false, None) }
                .map_err(|error| init_error_from("CreateEventW(atlas watermark)", error))?;
            // SAFETY: fence / 事件句柄在本次调用期间有效。
            let armed = unsafe { fence.SetEventOnCompletion(value, event) };
            if let Err(error) = armed {
                // SAFETY: 事件句柄由本函数独占，失败路径也必须关闭一次。
                unsafe {
                    let _ = CloseHandle(event);
                }
                return Err(init_error_from(
                    "SetEventOnCompletion(atlas watermark)",
                    error,
                ));
            }
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1) as u32;
            // SAFETY: 事件句柄在本次调用期间有效。
            let wait = unsafe { WaitForSingleObject(event, remaining) };
            // SAFETY: 事件句柄由本函数独占，关闭一次。
            unsafe {
                let _ = CloseHandle(event);
            }
            if wait == WAIT_OBJECT_0 && unsafe { fence.GetCompletedValue() } >= value {
                continue;
            }
            // 设备真的没了 ⇒ 走设备丢失路径；否则是本轮慢路径（合成器/GPU 停摆）。
            // SAFETY: 设备句柄由本结构持有，调用期间有效；查询无副作用。
            if let Err(error) = unsafe { self.raw.GetDeviceRemovedReason() }
                && is_device_removal(error.code())
                && let Some(sticky) = self.note_failure()
            {
                return Err(sticky);
            }
            synced = false;
            super::diagnostics::frame_wait_timeout();
            if wait == WAIT_TIMEOUT || Instant::now() >= deadline {
                break;
            }
        }
        Ok(synced)
    }

    /// 登记资源（`&mut Device` 的短临界区）。
    pub(crate) fn allocate(&self, resource: ID3D12Resource) -> GpuResourceHandle {
        self.device_lock().allocate(resource)
    }

    /// 释放句柄对应的资源；陈旧句柄返回 `false`。
    pub(crate) fn release(&self, handle: GpuResourceHandle) -> bool {
        self.device_lock().release(handle)
    }

    /// 任一 D3D12 调用失败后的分类（设备丢失是**进程级**粘性状态：一个窗口检测到移除之后，
    /// 其余窗口在下一次提交 / 等待时拿到同一个 `DeviceLost`）。
    pub(crate) fn note_failure(&self) -> Option<EngineError> {
        self.device_lock().note_failure()
    }

    /// 粘性丢失错误（`Some` 之后不得再触碰 D3D12）。
    pub(crate) fn lost_error(&self) -> Option<EngineError> {
        self.device_lock().lost_error()
    }

    /// 测试：共享设备句柄表里仍存活的资源数。
    #[cfg(test)]
    pub(crate) fn live_resources(&self) -> usize {
        self.device_lock().live_resources()
    }

    /// 把一个或多个图集页重建到指定页尺寸。
    ///
    /// 写锁内重新检查（排队期间别的窗口可能已经建好同一页），然后**等各窗口的提交水位**：
    /// 页重建是唯一的「全局事件」，但它需要的不是整条队列 idle，而是「没有在飞命令列表还引用
    /// 旧纹理」（R08-6）。所有窗口的录制 + 提交都在图集**读锁**内完成，写锁期间不可能出现新
    /// 提交，因此快照时的水位就覆盖了全部在飞引用。
    ///
    /// 页重建是**进程级事件**：只有第一个看到「App 换代的新页」的窗口会真正重建（窗口侧的
    /// `AtlasUploads::reconcile_pages` 已经把「别的窗口换代之后留下的旧页上传」丢弃），其余
    /// 窗口在写锁释放后发现页已对齐，直接录制自己的上传。
    pub(crate) fn rebuild_atlas_pages(
        &self,
        requests: &[(AtlasKind, u32)],
    ) -> Result<(), EngineError> {
        let mut atlas = self.atlas.write().unwrap_or_else(PoisonError::into_inner);
        let pending: Vec<(AtlasKind, u32)> = requests
            .iter()
            .copied()
            .filter(|(kind, page)| atlas.page(*kind) != *page)
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let synced = self.wait_watermarks()?;
        if synced {
            // 上一次慢路径留下的旧纹理此时一定没有引用（所有水位都到了）。
            self.deferred_atlas
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        } else {
            super::diagnostics::atlas_deferred_rebuild();
        }
        let mut rebuilt = AtlasRebuilt::default();
        for (kind, page) in pending {
            let old = atlas.recreate(self.raw(), kind, page)?;
            if synced {
                drop(old);
            } else {
                // 慢路径：仍可能有在飞帧引用旧纹理（只是等不到水位）。留在表里，下一次成功
                // 同步时释放 —— 绝不在这里读已销毁的 UPLOAD/纹理。
                self.deferred_atlas
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(old);
            }
            self.srv_heap
                .write_atlas(self.raw(), kind, atlas.texture(kind));
            self.atlas_pages[kind.index() as usize].store(page, Ordering::Release);
            rebuilt.mark(kind);
        }
        debug_assert!(
            rebuilt.glyph || rebuilt.image,
            "a non-empty rebuild must recreate at least one atlas page"
        );
        Ok(())
    }

    /// 设备恢复后按 CPU 影子页重传图集（R08-4）。
    ///
    /// 影子页是录制期从同一份上传字节落下来的（`SharedAtlas::update_shadow`），因此恢复后的
    /// 图集内容与被移除设备上的一致。
    fn restore_atlas(&self, snapshot: &[Option<(u32, Vec<u8>)>; 2]) -> Result<(), EngineError> {
        let mut atlas = self.atlas.write().unwrap_or_else(PoisonError::into_inner);
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let Some((page, pixels)) = snapshot[index].as_ref() else {
                continue;
            };
            if *page <= 1 {
                continue;
            }
            let row_bytes = *page as usize * texel_bytes(kind) as usize;
            if pixels.len() != row_bytes * *page as usize {
                continue;
            }
            atlas.recreate(self.raw(), kind, *page)?;
            self.srv_heap
                .write_atlas(self.raw(), kind, atlas.texture(kind));
            self.atlas_pages[index].store(*page, Ordering::Release);
            let texture = atlas.texture(kind).clone();
            self.upload_atlas_page_now(&texture, kind, *page, pixels)?;
        }
        Ok(())
    }

    /// 帧外的一次性整页上传（只用于设备恢复：新设备上的图集是空纹理）。
    ///
    /// 自己建 staging / allocator / list / fence 并同步等完成：此刻还没有任何窗口的提交流
    /// （恢复发生在窗口重建之前），不需要与它们同步。
    fn upload_atlas_page_now(
        &self,
        texture: &ID3D12Resource,
        kind: AtlasKind,
        page: u32,
        pixels: &[u8],
    ) -> Result<(), EngineError> {
        let row_bytes = page as usize * texel_bytes(kind) as usize;
        let row_pitch = row_pitch_for(kind, page);
        let staging = create_staging(self.raw(), u64::from(row_pitch) * u64::from(page))?;
        let mut pointer: *mut c_void = std::ptr::null_mut();
        // SAFETY: UPLOAD 堆；本函数独占该映射直到资源释放。
        unsafe { staging.Map(0, None, Some(&mut pointer)) }
            .map_err(|error| init_error_from("Map(atlas restore)", error))?;
        for row in 0..page as usize {
            // SAFETY: staging 容量 = `row_pitch × page`；源行在 `pixels` 内（长度已校验）。
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(row * row_bytes),
                    pointer.cast::<u8>().add(row * row_pitch as usize),
                    row_bytes,
                );
            }
        }
        // SAFETY: 与上面的 `Map` 成对。
        unsafe { staging.Unmap(0, None) };

        // SAFETY: 设备句柄有效；分配器/列表都是新建的，独占使用。
        let allocator: ID3D12CommandAllocator = unsafe {
            self.raw
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
        }
        .map_err(|error| init_error_from("CreateCommandAllocator(atlas restore)", error))?;
        // SAFETY: 同上。
        let list: ID3D12GraphicsCommandList = unsafe {
            self.raw
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
        }
        .map_err(|error| init_error_from("CreateCommandList(atlas restore)", error))?;
        super::atlas::transition(
            &list,
            texture,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        super::atlas::copy_page(&list, texture, &staging, kind, page, row_pitch);
        super::atlas::transition(
            &list,
            texture,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        // SAFETY: 列表由本函数独占，录制完成。
        unsafe { list.Close() }.map_err(|error| init_error_from("Close(atlas restore)", error))?;
        let command_list: ID3D12CommandList = list
            .cast()
            .map_err(|error| init_error_from("cast ID3D12CommandList", error))?;
        // SAFETY: 命令列表已关闭；队列是进程级的。
        unsafe { self.queue.ExecuteCommandLists(&[Some(command_list)]) };
        // SAFETY: 新 fence，signal 1；事件仅本次等待使用。
        let fence: ID3D12Fence = unsafe { self.raw.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
            .map_err(|error| init_error_from("CreateFence(atlas restore)", error))?;
        // SAFETY: fence 由本函数持有；Signal 只入队。
        unsafe { self.queue.Signal(&fence, 1) }
            .map_err(|error| init_error_from("Signal(atlas restore)", error))?;
        // SAFETY: 无名称 auto-reset 事件；句柄在返回前关闭。
        let event: HANDLE = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| init_error_from("CreateEventW(atlas restore)", error))?;
        // SAFETY: fence / 事件句柄在本次调用期间有效。
        let armed = unsafe { fence.SetEventOnCompletion(1, event) };
        if let Err(error) = armed {
            // SAFETY: 事件句柄由本函数独占。
            unsafe {
                let _ = CloseHandle(event);
            }
            return Err(init_error_from(
                "SetEventOnCompletion(atlas restore)",
                error,
            ));
        }
        // SAFETY: 同上。
        let wait = unsafe { WaitForSingleObject(event, FENCE_TIMEOUT_MS) };
        // SAFETY: 事件句柄由本函数独占。
        unsafe {
            let _ = CloseHandle(event);
        }
        if wait != WAIT_OBJECT_0 {
            // 恢复路径上的超时：设备健康 ⇒ 放弃这次图集重传（窗口照常恢复，图集内容下一帧由
            // 上传路径补齐）；设备移除 ⇒ 按设备丢失处理。
            return match self.note_failure() {
                Some(sticky) => Err(sticky),
                None => Ok(()),
            };
        }
        Ok(())
    }

    fn device_lock(&self) -> MutexGuard<'_, Device> {
        self.device.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for SharedGpu {
    fn drop(&mut self) {
        // 进程 teardown：把 InfoQueue 里剩下的消息计数并落日志（R08-5）。release 构建是空实现。
        let counts = debug::drain_messages(&self.raw);
        if counts.warning + counts.error + counts.corruption > 0 {
            eprintln!(
                "kytrix: d3d12 debug layer totals: warnings={} errors={} corruption={}",
                counts.warning, counts.error, counts.corruption
            );
        }
    }
}

/// 一个窗口在共享 SRV 堆里的段（`Drop` 时把段归还给 `DescriptorArena`）。
///
/// `Renderer` 在 GPU 线程 join 之后才销毁，因此归还时该窗口的描述符不再被任何在飞命令列表
/// 引用；新窗口拿到同一段后重写描述符是安全的。
pub(crate) struct DescriptorLease {
    gpu: Arc<SharedGpu>,
    base: u32,
}

impl DescriptorLease {
    /// 段内第一个描述符的绝对索引（`SrvHeap::slot_table` / `SrvHeap::write_slot` 用）。
    pub(crate) fn base(&self) -> u32 {
        self.base
    }
}

impl Drop for DescriptorLease {
    fn drop(&mut self) {
        self.gpu.release_window_descriptors(self.base);
    }
}

/// 进程级共享 GPU 的注册表（**可替换**，R08-4）。
///
/// 旧实现是 `OnceLock<Arc<SharedGpu>>`：设备真的被移除之后进程里再没有可用的设备。
/// 现在把「当前实例」放进 `Mutex`，并记一个限次窗口给恢复用。
struct GpuCell {
    current: Option<Weak<SharedGpu>>,
    /// 最近 [`RECOVERY_WINDOW`] 内的恢复尝试时刻。
    recoveries: VecDeque<Instant>,
    /// 进程世代号（每次创建 / 恢复 +1；日志字段）。
    generation: u64,
}

/// 恢复尝试的时间窗口与上限（R08-4：3 次 / 5 s）。
const RECOVERY_WINDOW: Duration = Duration::from_secs(5);
const MAX_RECOVERIES_PER_WINDOW: usize = 3;

fn gpu_cell() -> MutexGuard<'static, GpuCell> {
    static CELL: OnceLock<Mutex<GpuCell>> = OnceLock::new();
    CELL.get_or_init(|| {
        Mutex::new(GpuCell {
            current: None,
            recoveries: VecDeque::new(),
            generation: 0,
        })
    })
    .lock()
    .unwrap_or_else(PoisonError::into_inner)
}

/// 进程级单例的创建协议：快路径无锁，慢路径在初始化互斥锁内二次检查（WARP 用）。
#[cfg(any(test, feature = "test-support"))]
fn shared_instance(
    cell: &'static OnceLock<Arc<SharedGpu>>,
    init: &'static Mutex<()>,
    create: impl FnOnce() -> Result<SharedGpu, EngineError>,
) -> Result<Arc<SharedGpu>, EngineError> {
    if let Some(gpu) = cell.get() {
        return Ok(Arc::clone(gpu));
    }
    let _guard = init.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(gpu) = cell.get() {
        return Ok(Arc::clone(gpu));
    }
    let gpu = Arc::new(create()?);
    let _ = cell.set(Arc::clone(&gpu));
    Ok(gpu)
}

/// 选硬件适配器：优先**高性能**（独显），再退回「第一个非软件适配器」。
///
/// 混合显卡机器（笔记本 / 台式双卡）上 `EnumAdapters1` 的枚举顺序通常是 iGPU 在前，
/// 直接取第一个非软件适配器会静默用上核显。D3D12 官方推荐的做法是
/// `EnumAdapterByGpuPreference(DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)`：它会挑驱动声明的
/// 高性能适配器（无独显时等价于默认顺序）。
///
/// 保留旧枚举作为兜底：`IDXGIFactory6` 不可用（极老的 DXGI）时仍能拿到一个硬件适配器。
fn pick_hardware_adapter(factory: &IDXGIFactory4) -> Result<IDXGIAdapter1, EngineError> {
    // `cast` 是安全的 COM 查询（失败只说明 IDXGIFactory6 不可用）。
    if let Ok(factory6) = factory.cast::<IDXGIFactory6>() {
        // SAFETY: 句柄由调用方持有，参数与输出结构在调用期间有效。
        if let Ok(adapter) = unsafe {
            factory6.EnumAdapterByGpuPreference::<IDXGIAdapter1>(
                0,
                DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
            )
        } {
            // SAFETY: 适配器句柄由本函数持有到返回。
            let desc = unsafe { adapter.GetDesc1() }
                .map_err(|error| init_error_from("GetDesc1", error))?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                return Ok(adapter);
            }
        }
    }
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
        // SAFETY: COM 调用：句柄有效，参数是值。
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

    // R08-12：本后端唯一依赖的非通用能力（relaxed format casting）。
    let mut options3 = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
    check_feature(
        device,
        D3D12_FEATURE_D3D12_OPTIONS3,
        &mut options3,
        std::mem::size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS3>(),
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
        casting_fully_typed: options3.CastingFullyTypedFormatSupported.as_bool(),
        debug_layer: debug::layer_available(),
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

/// 一块 UPLOAD 堆缓冲（设备恢复时的整页 staging）。
fn create_staging(device: &ID3D12Device, bytes: u64) -> Result<ID3D12Resource, EngineError> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.max(4),
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
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
            &mut out,
        )
    }
    .map_err(|error| init_error_from("CreateCommittedResource(staging)", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: SmolStr::new_static("d3d12 staging buffer missing"),
    })
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

/// GPU 测试的进程级串行锁：多个 WARP 设备 / 多个 GPU 线程并行会互相拖垮（本机实测 device removed）。
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

    use super::{Device, SharedGpu};
    use crate::{error::EngineError, paint::atlas::AtlasKind};

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
        // SAFETY: COM 调用：设备句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
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
        assert!(device.caps().describe().contains("warp=true"));
        assert!(
            device.caps().casting_fully_typed,
            "WARP 支持 relaxed format casting；不支持时 D8 必须显式失败"
        );
    }

    /// 进程级单例：两次取到的是同一个 `SharedGpu`（设备 / queue / PSO / 图集只创建一次）。
    #[test]
    fn the_process_wide_gpu_is_created_once() {
        let _guard = super::gpu_test_lock();
        let first = SharedGpu::process_warp().expect("WARP shared gpu");
        let second = SharedGpu::process_warp().expect("WARP shared gpu");
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(
            first.atlas().page(AtlasKind::Glyph),
            second.atlas().page(AtlasKind::Glyph)
        );
    }

    /// 共享图集的页换代：写锁 + 各窗口水位之后替换纹理，页尺寸与描述符一起前进。
    #[test]
    fn rebuilding_a_shared_atlas_page_swaps_the_texture() {
        let _guard = super::gpu_test_lock();
        let shared = SharedGpu::new_warp().expect("WARP shared gpu");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 1, "占位页是 1×1");
        shared
            .rebuild_atlas_pages(&[(AtlasKind::Glyph, 64)])
            .expect("rebuild");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 64);
        assert_eq!(shared.atlas().row_pitch(AtlasKind::Glyph), 256);

        // 同一页重复请求是 no-op（其余窗口在写锁释放后不必再重建）。
        shared
            .rebuild_atlas_pages(&[(AtlasKind::Glyph, 64)])
            .expect("no-op rebuild");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 64);
    }

    /// 设备真正被移除时：`note_failure` 升级为 `DeviceLost`（粘性）并落 DRED 日志。
    ///
    /// **必须在独立进程里跑**：`RemoveDevice` 是运行时级操作，会污染同进程所有 D3D12 设备
    /// （V-59；旧实现每窗口一个设备时没有这个问题）。而本测试进程里已经有进程级共享 GPU，
    /// 所以这里把会真正移除设备的用例放子进程执行，父进程只断言子进程退出码。
    #[test]
    fn removed_device_reports_device_lost_and_writes_a_log() {
        const CHILD_ENV: &str = "KYTRIX_DEVICE_LOST_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let exe = std::env::current_exe().expect("the test binary path");
            let status = std::process::Command::new(exe)
                .arg("--exact")
                .arg(concat!(
                    "render::windows::device::tests::",
                    "removed_device_reports_device_lost_and_writes_a_log"
                ))
                .arg("--nocapture")
                .env(CHILD_ENV, "1")
                .status()
                .expect("spawn the device-lost child process");
            assert!(
                status.success(),
                "the device-lost child process must pass: {status}"
            );
            return;
        }
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
                assert!(
                    reason.contains("DEVICE_REMOVED"),
                    "原因短名必须可读: {reason}"
                );
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

    /// R08-4：新实例的世代号与旧实例无关；注册表在有健康实例时**不**新建（进程级复用）。
    #[test]
    fn the_gpu_cell_reuses_a_healthy_instance() {
        let _guard = super::gpu_test_lock();
        let first = SharedGpu::new_warp().expect("WARP shared gpu");
        let before = super::gpu_cell().generation;
        // `process_warp` 是另一个单例，不应影响进程级注册表。
        let _ = SharedGpu::process_warp().expect("WARP singleton");
        assert_eq!(super::gpu_cell().generation, before);
        assert!(std::sync::Arc::strong_count(&first) >= 1);
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
            casting_fully_typed: true,
            debug_layer: false,
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

        // R08-12：不支持 relaxed format casting 的驱动（RS1）必须**显式**失败，而不是
        // 悄悄输出「整窗偏暗」的像素。
        let mut no_cast = caps();
        no_cast.casting_fully_typed = false;
        assert!(!no_cast.baseline_ok());
        match no_cast.check().expect_err("casting is a hard requirement") {
            EngineError::UnsupportedGpu { actual, .. } => {
                assert!(actual.contains("CastSrgb false"), "{actual}")
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn check_reports_required_and_actual() {
        let mut below = caps();
        below.feature_level = 45_056;
        below.shader_model = 96;
        below.resource_binding_tier = 1;
        below.casting_fully_typed = false;

        let error = below.check().expect_err("baseline must fail");
        match &error {
            EngineError::UnsupportedGpu {
                os,
                required,
                actual,
            } => {
                assert_eq!(*os, "windows");
                assert_eq!(*required, D8_REQUIRED);
                assert_eq!(
                    actual.as_str(),
                    "FL 11_0 / SM 6_0 / BindingTier 1 / CastSrgb false"
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        let text = error.to_string();
        assert!(
            text.contains("FL 11_0 / SM 6_0 / BindingTier 1 / CastSrgb false"),
            "{text}"
        );
        assert!(text.contains("release notes"), "{text}");
        assert!(!text.to_lowercase().contains("falling back"), "{text}");
        assert!(
            !text.to_lowercase().contains("software rendering"),
            "{text}"
        );
        assert!(
            D8_REQUIRED.contains("CastingFullyTypedFormat"),
            "D8 文案必须列出 cast 能力位: {D8_REQUIRED}"
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
            "cast_srgb=true",
            "debug_layer=false",
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

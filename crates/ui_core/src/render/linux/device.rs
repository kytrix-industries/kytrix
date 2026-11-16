//! 进程级共享 Vulkan 设备（[`SharedGpu`]）+ 每窗口帧同步原语（[`FrameSync`]）
//! （`design/phase-12-macos-linux.md` §6.2、`design/async-render-pipeline.md` §7.2）。
//!
//! 多窗口的正确形状（与 Chrome / Qt RHI 一致）：进程一个 `VkInstance`、一个
//! `VkPhysicalDevice`、一个 `VkDevice`、一条 graphics queue、一套 render pass / PSO、两张
//! 图集纹理；每个窗口只保留 `VkSurfaceKHR` / swapchain / 持久目标 / fence 与帧槽 / 上传缓冲 /
//! 描述符段。`VkInstance` / `VkDevice` 的创建与销毁**引用计数**：进程级单例表只存 `Weak`，
//! 最后一个窗口的 `Arc` drop 时才销毁（顺序固定为 swapchain → surface → device → instance）。
//!
//! 共享状态全部有明确的同步策略：
//!
//! - **队列**（[`SharedGpu::with_queue`]）：Vulkan 要求 `VkQueue` 的主机访问外部同步，所有
//!   submit / present 都在一把进程级互斥锁内串行化到同一条 queue。选择「单 graphics queue +
//!   提交串行化」而不是每窗口队列：与 Windows 后端同构（那里 D3D12 队列本身 free-threaded），
//!   Vulkan 侧只是一把短临界区锁，提交本身仍然异步（I2）。
//! - **资源句柄表 / 设备丢失状态**（`Mutex`）：窗口线程 allocate / release 的短临界区；设备
//!   丢失是进程级粘性状态。
//! - **图集纹理**（`RwLock`，见 [`SharedAtlas`]）：窗口在**读锁**内录制并提交一帧；页重建走
//!   **写锁**并先做有界队列 idle（[`SharedGpu::wait_queue_idle`]），之后才替换纹理、重写全部
//!   窗口描述符段。多窗口同时上传同一纹理因此是安全的：同一条队列上的命令按提交顺序执行，
//!   上传的 `SHADER_READ_ONLY → TRANSFER_DST → SHADER_READ_ONLY` 屏障序列不会交错。
//! - **描述符池**（`Mutex`）：`vkUpdateDescriptorSets` 要求 descriptor pool 外部同步；窗口写
//!   自己的实例缓冲描述符、图集换代写全部窗口的图集绑定，都在这把锁内。
//! - **render pass / PSO / 队列族 / 内存属性** 创建后只读。
//!
//! 每窗口的同步原语（[`FrameSync`]）**不是**共享的：timeline 的 signal 值按窗口自己的帧序号
//! 单调递增，多窗口共享同一个 timeline 会互相覆盖；fence / `image_available` 同理（一个是
//! 帧槽屏障、一个是 acquire 信号量）。retire 队列也是每窗口的：句柄来自共享设备句柄表，但
//! 「哪一帧完成、何时释放」由各窗口自己的 timeline 决定。

use std::{
    ffi::c_void,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock, PoisonError, RwLock, RwLockReadGuard, Weak,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use ash::vk;
use smol_str::SmolStr;

use super::{
    atlas::SharedAtlas,
    instance::VulkanInstance,
    pipeline::{DescriptorPool, MAX_WINDOWS, Pipeline},
};
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        atlas::AtlasRebuilt,
        frame::{FrameSlot, InFlightRing, MAX_FRAMES_IN_FLIGHT},
        retire::{GpuResourceHandle, RetireQueue},
    },
    window::FrameIndex,
};

/// D8 硬基线（错误文案与安装器共用的唯一来源）。
pub(crate) const D8_REQUIRED: &str = "Vulkan 1.2 / descriptorIndexing / timelineSemaphore / VK_KHR_swapchain / VK_KHR_wayland_surface";

/// 有界 GPU 等待的上限（页重建 idle / fence / timeline）。GPU watchdog 的常规量级是 1–2 s，
/// 健康的帧远达不到；INFINITE 会在设备挂死时永久阻塞。
pub(crate) const GPU_TIMEOUT_NS: u64 = 2_000_000_000;

/// 启动时一次性采集的能力快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuCaps {
    /// `VkPhysicalDeviceProperties.deviceName`。
    pub adapter: SmolStr,
    /// `apiVersion`（原始 `u32`，`actual()` 里格式化）。
    pub api_version: u32,
    pub device_type: vk::PhysicalDeviceType,
    pub descriptor_indexing: bool,
    pub runtime_descriptor_array: bool,
    pub timeline_semaphore: bool,
    pub swapchain: bool,
    /// `VK_KHR_wayland_surface`（产品实例强制启用；离屏实例没有 WSI，为 `false`）。
    pub wayland_surface: bool,
    pub incremental_present: bool,
}

impl GpuCaps {
    /// 硬基线：Vulkan 1.2 + descriptorIndexing + timelineSemaphore + swapchain。
    pub(crate) fn baseline_ok(&self) -> bool {
        self.api_version >= vk::API_VERSION_1_2
            && self.descriptor_indexing
            && self.timeline_semaphore
    }

    /// `UnsupportedGpu.actual` 的稳定格式（只含基线项）。
    pub(crate) fn actual(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "Vulkan {}.{}.{} / descriptorIndexing={} / timelineSemaphore={}",
            vk::api_version_major(self.api_version),
            vk::api_version_minor(self.api_version),
            vk::api_version_patch(self.api_version),
            self.descriptor_indexing,
            self.timeline_semaphore
        )
    }

    /// 全字段诊断行（不含时间戳）。
    pub(crate) fn describe(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "adapter={:?} api={}.{}.{} type={:?} descriptor_indexing={} runtime_array={} timeline={} swapchain={} wayland_surface={} incremental_present={}",
            self.adapter,
            vk::api_version_major(self.api_version),
            vk::api_version_minor(self.api_version),
            vk::api_version_patch(self.api_version),
            self.device_type,
            self.descriptor_indexing,
            self.runtime_descriptor_array,
            self.timeline_semaphore,
            self.swapchain,
            self.wayland_surface,
            self.incremental_present
        )
    }

    /// 唯一校验入口（I10：不降级）。
    pub(crate) fn check(&self) -> Result<(), EngineError> {
        if self.baseline_ok() {
            Ok(())
        } else {
            Err(EngineError::UnsupportedGpu {
                os: "linux",
                required: D8_REQUIRED,
                actual: self.actual(),
            })
        }
    }
}

/// `GpuResourceHandle` 的 `kind`：Vulkan 资源（image / buffer / memory / view / sampler）。
const HANDLE_KIND_OBJECT: u8 = 1;

thread_local! {
    /// 当前线程是否是 `kytrix-vulkan` GPU 线程（`design/async-render-pipeline.md` §10.1 的归属断言）。
    static ON_GPU_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 标记当前线程为 GPU 线程（`render/linux/renderer.rs` 的循环入口调用一次）。
pub(crate) fn enter_gpu_thread() {
    ON_GPU_THREAD.set(true);
}

/// 所有**窗口路径**的 Vulkan 调用都必须在 GPU 线程上（对象构造阶段除外，§3.1）。
///
/// 离屏 / 快照路径由门面在 UI 线程同步调用，因此断言只放在「窗口目标」的分支上。
pub(crate) fn on_gpu_thread() -> bool {
    ON_GPU_THREAD.get()
}

/// 句柄表里的 Vulkan 资源（`GpuResourceHandle` 的载荷）。
#[derive(Clone, Copy, Debug)]
pub(crate) enum GpuObject {
    Buffer(vk::Buffer),
    Memory(vk::DeviceMemory),
}

impl GpuObject {
    /// 销毁底层 Vulkan 对象（[`SharedGpu::release`] 的唯一出口）。
    fn destroy(self, device: &ash::Device) {
        // SAFETY: 句柄归本枚举独占，且调用方保证 GPU 不再使用它。
        unsafe {
            match self {
                Self::Buffer(buffer) => device.destroy_buffer(buffer, None),
                Self::Memory(memory) => device.free_memory(memory, None),
            }
        }
    }
}

/// 一个逻辑设备 + 一条 graphics queue + 能力 / 内存快照。
///
/// 它是**进程级**对象（由 [`SharedGpu`] 持有）。每窗口的 fence / 信号量 / timeline 在
/// [`FrameSync`] 里，不在这里。
pub(crate) struct Device {
    raw: ash::Device,
    physical: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family: u32,
    caps: GpuCaps,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    /// `VkPhysicalDeviceLimits.timestampPeriod`（ns/tick）。
    timestamp_period: f32,
    /// `VkQueueFamilyProperties.timestampValidBits`（0 = 不支持时间戳）。
    timestamp_valid_bits: u32,
}

impl Device {
    /// 选卡（有 Wayland surface 支持的最高分设备）→ 采 features/扩展 → 建 device → 校验 D8。
    ///
    /// `pick_surface` 是设备选卡用的探针 surface（产品窗口路径）或 `None`（离屏路径）：
    /// 选卡逻辑（按 `DEVICE_TYPE` 打分、`no Vulkan device with a Wayland surface queue` 错误
    /// 文案）保持不变，只是现在进程级只执行一次。
    fn new(
        instance: &VulkanInstance,
        pick_surface: Option<vk::SurfaceKHR>,
    ) -> Result<Self, EngineError> {
        // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
        let devices = unsafe { instance.raw().enumerate_physical_devices() }
            .map_err(|error| init_error("enumerate_physical_devices", format!("{error:?}")))?;
        let surface_fn = instance.surface_fn();
        // **打分选设备**而不是取第一个能用的（P2-3.1）：旧实现在混合显卡机器上可能选中
        // iGPU，也可能选中 lavapipe（CPU 软件光栅器）—— 前者慢，后者让「有独显却在软件
        // 渲染」这种问题完全不报。
        //
        // 打分顺序：独显 3 > 集显 2 > 虚拟/其它 1 > CPU 软件光栅器 0。**CPU 不排除**：
        // 无头 CI / 远程主机常常只有 lavapipe，把它彻底拒掉会让引擎在那类环境里完全跑不起来
        // （Chrome 同样保留 SwiftShader 作为兜底）。硬件与软件都在时硬件必然胜出。
        let mut best: Option<(u32, vk::PhysicalDevice, u32)> = None;
        for physical in devices {
            // SAFETY: 句柄由本结构独占，只读查询。
            let properties = unsafe { instance.raw().get_physical_device_properties(physical) };
            let score = match properties.device_type {
                vk::PhysicalDeviceType::CPU => 0,
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                _ => 1,
            };
            // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
            let family = unsafe {
                instance
                    .raw()
                    .get_physical_device_queue_family_properties(physical)
            }
            .iter()
            .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS));
            let Some(family) = family else {
                continue;
            };
            // 离屏实例没有 surface：只要求图形队列族（`pick_surface` 为 `None`）。
            let supported = match (surface_fn, pick_surface) {
                (Some(surface_fn), Some(surface)) => {
                    // SAFETY: `family` 来自本设备的队列族列表；surface 由同一实例创建。
                    unsafe {
                        surface_fn.get_physical_device_surface_support(
                            physical,
                            family as u32,
                            surface,
                        )
                    }
                    .unwrap_or(false)
                }
                _ => true,
            };
            if !supported {
                continue;
            }
            if best.is_none_or(|(best_score, _, _)| score > best_score) {
                best = Some((score, physical, family as u32));
            }
        }
        let (_, physical, queue_family) = best.ok_or(EngineError::UnsupportedGpu {
            os: "linux",
            required: D8_REQUIRED,
            actual: SmolStr::new_static("no Vulkan device with a Wayland surface queue"),
        })?;

        // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
        let properties = unsafe { instance.raw().get_physical_device_properties(physical) };
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let device_extensions = unsafe {
            instance
                .raw()
                .enumerate_device_extension_properties(physical)
        }
        .unwrap_or_default();
        let has_extension = |name: &std::ffi::CStr| {
            device_extensions.iter().any(|extension| {
                // SAFETY: `extension_name` 是 Vulkan 保证的 NUL 结尾字符串。
                (unsafe { std::ffi::CStr::from_ptr(extension.extension_name.as_ptr()) }) == name
            })
        };
        let swapchain = has_extension(ash::khr::swapchain::NAME);
        let incremental_present = has_extension(ash::khr::incremental_present::NAME);

        let mut features12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut features12);
        // SAFETY: features 链的 `pNext` 指向本函数栈上的 `features12`。
        unsafe {
            instance
                .raw()
                .get_physical_device_features2(physical, &mut features2);
        }
        let caps = GpuCaps {
            adapter: SmolStr::new(
                // SAFETY: `deviceName` 是 Vulkan 保证的 NUL 结尾字符串。
                unsafe { std::ffi::CStr::from_ptr(properties.device_name.as_ptr()) }
                    .to_string_lossy(),
            ),
            api_version: properties.api_version,
            device_type: properties.device_type,
            descriptor_indexing: features12.descriptor_indexing == vk::TRUE,
            runtime_descriptor_array: features12.runtime_descriptor_array == vk::TRUE,
            timeline_semaphore: features12.timeline_semaphore == vk::TRUE,
            swapchain,
            // 产品实例强制 `VK_KHR_wayland_surface`（`VulkanInstance::new_product`）；
            // 离屏实例没有 WSI 函数表。
            wayland_surface: instance.surface_fn().is_some(),
            incremental_present,
        };
        caps.check()?;
        // 窗口路径需要 present：`VK_KHR_swapchain` 缺失是硬失败。
        // 离屏实例（快照）没有 surface，自然没有 swapchain：那条路径不需要 present。
        let window_path = pick_surface.is_some();
        if window_path && !caps.swapchain {
            return Err(EngineError::UnsupportedGpu {
                os: "linux",
                required: D8_REQUIRED,
                actual: SmolStr::new_static("VK_KHR_swapchain missing"),
            });
        }
        debug_assert!(!caps.describe().is_empty());

        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let mut enabled12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(true)
            .descriptor_indexing(true)
            .runtime_descriptor_array(caps.runtime_descriptor_array);
        // `VK_KHR_swapchain` 依赖实例扩展 `VK_KHR_surface`；离屏实例（快照 / 单测）没有
        // surface，启用该设备扩展会被 validation 判为缺少依赖（VUID-vkCreateDevice-…-01387）。
        let mut extension_pointers = Vec::with_capacity(2);
        if caps.swapchain && window_path {
            extension_pointers.push(ash::khr::swapchain::NAME.as_ptr());
            if incremental_present {
                extension_pointers.push(ash::khr::incremental_present::NAME.as_ptr());
            }
        }
        let create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&extension_pointers)
            .push_next(&mut enabled12);
        // SAFETY: `create` 的 `pNext` 指向本函数栈上的 `enabled12`。
        let raw = unsafe { instance.raw().create_device(physical, &create, None) }
            .map_err(|error| init_error("create_device", format!("{error:?}")))?;
        // SAFETY: 队列族在创建时已校验存在。
        let queue = unsafe { raw.get_device_queue(queue_family, 0) };
        let memory_properties = unsafe {
            instance
                .raw()
                .get_physical_device_memory_properties(physical)
        };
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let queue_properties = unsafe {
            instance
                .raw()
                .get_physical_device_queue_family_properties(physical)
        };
        let timestamp_valid_bits = queue_properties
            .get(queue_family as usize)
            .map_or(0, |family| family.timestamp_valid_bits);

        Ok(Self {
            raw,
            physical,
            queue,
            queue_family,
            caps,
            memory_properties,
            timestamp_period: properties.limits.timestamp_period,
            timestamp_valid_bits,
        })
    }

    pub(crate) fn raw(&self) -> &ash::Device {
        &self.raw
    }

    pub(crate) fn physical(&self) -> vk::PhysicalDevice {
        self.physical
    }

    pub(crate) fn queue(&self) -> vk::Queue {
        self.queue
    }

    pub(crate) fn queue_family(&self) -> u32 {
        self.queue_family
    }

    pub(crate) fn caps(&self) -> &GpuCaps {
        &self.caps
    }

    /// `timestampPeriod`（ns/tick）与队列的时间戳有效位数（0 = 不支持）。
    pub(crate) fn timestamps(&self) -> (f32, u32) {
        (self.timestamp_period, self.timestamp_valid_bits)
    }

    /// 选一个满足 `required` 的内存类型（`HOST_VISIBLE` 之类）。
    pub(crate) fn memory_type(
        &self,
        type_bits: u32,
        required: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        (0..self.memory_properties.memory_type_count).find(|index| {
            let supported = type_bits & (1 << index) != 0;
            let flags = self.memory_properties.memory_types[*index as usize].property_flags;
            supported && flags.contains(required)
        })
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: 所有子资源（PSO / 图集 / 描述符池 / 每窗口 buffer 与 image）必须已先销毁：
        // `SharedGpu` 的字段声明顺序保证子资源先于 device drop，窗口资源由各窗口先释放。
        unsafe { self.raw.destroy_device(None) };
    }
}

/// 设备丢失的粘性记录。
struct LostInfo {
    reason: SmolStr,
    log_path: PathBuf,
}

/// 进程级共享 GPU：实例 + 设备 + 队列 + PSO + 描述符池 + 图集纹理 + 资源句柄表。
///
/// 构造函数是**引用计数**的进程级单例（[`SharedGpu::process_windowed`] /
/// [`SharedGpu::process_headless`]）；单例表只存 `Weak`，最后一个使用者 drop 之后整个
/// Vulkan 栈按 swapchain → surface → device → instance 的顺序销毁。
pub(crate) struct SharedGpu {
    /// 所有窗口的 descriptor set 都从这一个池预分配固定段（`DescriptorLease` 归还段）。
    descriptors: Mutex<DescriptorPool>,
    /// 进程唯一的 render pass / PSO 集合（创建后只读）。
    pipeline: Pipeline,
    /// 共享图集纹理：读锁 = 录制并提交一帧；写锁 = 页重建。
    atlas: RwLock<SharedAtlas>,
    /// 当前共享图集页尺寸的无锁快照（`[glyph, image]`；上传入队时打「入队页」标记用）。
    atlas_pages: [AtomicU32; 2],
    /// 共享队列的外部同步锁（Vulkan 要求 `VkQueue` 的主机访问外部同步）。
    queue_lock: Mutex<()>,
    /// 页重建时的全局 idle 观测点：写锁内入队一个 timeline signal 再等它完成（有界）。
    idle_timeline: vk::Semaphore,
    idle_sequence: AtomicU64,
    /// 资源句柄表（所有窗口的 buffer / memory；`Mutex` 只保护短临界区）。
    resources: Mutex<ResourceTable>,
    /// 设备丢失的进程级粘性状态。
    lost: Mutex<Option<LostInfo>>,
    // 销毁顺序（字段声明顺序 = drop 顺序）：子资源 → device → instance。
    device: Device,
    instance: VulkanInstance,
}

impl SharedGpu {
    /// 产品（窗口）路径的进程级共享 GPU：第一个窗口创建时初始化，后续窗口直接复用。
    ///
    /// 初始化失败不缓存（`EngineError` 不是 `Clone`，且失败后的重试成本远低于把进程钉在错误
    /// 状态上）：下一次窗口创建会重新尝试。
    pub(crate) fn process_windowed(
        display: *mut c_void,
        surface: *mut c_void,
    ) -> Result<Arc<Self>, EngineError> {
        static GPU: OnceLock<Mutex<Weak<SharedGpu>>> = OnceLock::new();
        let slot = GPU.get_or_init(|| Mutex::new(Weak::new()));
        Self::shared(slot, || Self::create_windowed(display, surface))
    }

    /// 离屏路径的进程级共享 GPU（无 WSI；`Window::enable_offscreen_renderer` / 像素快照）。
    pub(crate) fn process_headless() -> Result<Arc<Self>, EngineError> {
        static GPU: OnceLock<Mutex<Weak<SharedGpu>>> = OnceLock::new();
        let slot = GPU.get_or_init(|| Mutex::new(Weak::new()));
        Self::shared(slot, Self::create_headless)
    }

    /// 测试用：不进入进程级单例的独立共享 GPU（隔离图集 / 描述符段状态）。
    #[cfg(test)]
    pub(crate) fn new_headless_isolated() -> Result<Arc<Self>, EngineError> {
        Ok(Arc::new(Self::create_headless()?))
    }

    /// 引用计数单例的创建协议：快路径无锁，慢路径在初始化互斥锁内二次检查。
    fn shared(
        slot: &'static Mutex<Weak<SharedGpu>>,
        create: impl FnOnce() -> Result<SharedGpu, EngineError>,
    ) -> Result<Arc<Self>, EngineError> {
        let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(gpu) = slot.upgrade() {
            return Ok(gpu);
        }
        // 锁内创建设备（只有一次，~10 ms 量级）：避免两个窗口并发创建两套 Vulkan 栈。
        let gpu = Arc::new(create()?);
        *slot = Arc::downgrade(&gpu);
        Ok(gpu)
    }

    fn create_windowed(display: *mut c_void, surface: *mut c_void) -> Result<Self, EngineError> {
        let instance = VulkanInstance::new_product()?;
        // 探针 surface：设备选择要验证「有 Wayland surface 队列」（D8 的口径），但每个窗口的
        // `VkSurfaceKHR` 由窗口自己创建（surface 的存活期 = 该窗口）。
        let probe = instance.create_surface(display, surface)?;
        let device = match Device::new(&instance, Some(probe)) {
            Ok(device) => device,
            Err(error) => {
                instance.destroy_surface(probe);
                return Err(error);
            }
        };
        instance.destroy_surface(probe);
        Self::with_device(instance, device)
    }

    fn create_headless() -> Result<Self, EngineError> {
        let instance = VulkanInstance::new_headless()?;
        let device = Device::new(&instance, None)?;
        Self::with_device(instance, device)
    }

    fn with_device(instance: VulkanInstance, device: Device) -> Result<Self, EngineError> {
        let pipeline = Pipeline::new(device.raw())?;
        let atlas = SharedAtlas::new(&device)?;
        let descriptors = DescriptorPool::new(device.raw(), pipeline.set_layout())?;
        // 占位页也必须是 `SHADER_READ_ONLY_OPTIMAL`：否则第一笔上传之前采样的就是 UNDEFINED
        // 布局（validation 会报错）。真实页在 `rebuild_atlas_pages` 里同样先转布局。
        pipeline.write_atlas(
            descriptors.all_sets(),
            atlas.glyph_view(),
            atlas.image_view(),
            atlas.sampler(),
        );
        let idle_timeline = {
            let mut info = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            // SAFETY: `info` 是栈上值，只在本次创建期间被读。
            unsafe {
                device.raw().create_semaphore(
                    &vk::SemaphoreCreateInfo::default().push_next(&mut info),
                    None,
                )
            }
            .map_err(|error| init_error("create_semaphore(idle)", format!("{error:?}")))?
        };
        let atlas_pages = [
            AtomicU32::new(atlas.page(AtlasKind::Glyph)),
            AtomicU32::new(atlas.page(AtlasKind::Image)),
        ];
        let gpu = Self {
            descriptors: Mutex::new(descriptors),
            pipeline,
            atlas: RwLock::new(atlas),
            atlas_pages,
            queue_lock: Mutex::new(()),
            idle_timeline,
            idle_sequence: AtomicU64::new(0),
            resources: Mutex::new(ResourceTable::new()),
            lost: Mutex::new(None),
            device,
            instance,
        };
        {
            let atlas = gpu.atlas();
            gpu.transition_atlas_layout(&atlas, &[AtlasKind::Glyph, AtlasKind::Image])?;
        }
        Ok(gpu)
    }

    pub(crate) fn raw(&self) -> &ash::Device {
        self.device.raw()
    }

    pub(crate) fn physical(&self) -> vk::PhysicalDevice {
        self.device.physical()
    }

    pub(crate) fn queue_family(&self) -> u32 {
        self.device.queue_family()
    }

    pub(crate) fn caps(&self) -> &GpuCaps {
        self.device.caps()
    }

    pub(crate) fn timestamps(&self) -> (f32, u32) {
        self.device.timestamps()
    }

    pub(crate) fn memory_type(
        &self,
        type_bits: u32,
        required: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        self.device.memory_type(type_bits, required)
    }

    /// 进程级实例（每窗口 surface 的创建 / WSI 查询入口）。
    pub(crate) fn instance(&self) -> &VulkanInstance {
        &self.instance
    }

    /// 进程唯一的 render pass / PSO 集合。
    pub(crate) fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// 在共享队列的外部同步锁内执行一次主机访问（submit / present）；
    /// `VkQueue` 的 Vulkan 主机访问必须外部同步。
    pub(crate) fn with_queue<T>(&self, access: impl FnOnce(vk::Queue) -> T) -> T {
        let _guard = self
            .queue_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        access(self.device.queue())
    }

    /// 等共享队列上的所有已提交工作完成（图集页重建用）：写锁保证此刻没有窗口在录制 /
    /// 提交新工作，这里入队一个空提交只 signal timeline，然后**有界**等待。
    pub(crate) fn wait_queue_idle(&self) -> Result<(), EngineError> {
        let value = self.idle_sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let timeline = [self.idle_timeline];
        let values = [value];
        let mut timeline_info =
            vk::TimelineSemaphoreSubmitInfo::default().signal_semaphore_values(&values);
        let submit = vk::SubmitInfo::default()
            .signal_semaphores(&timeline)
            .push_next(&mut timeline_info);
        // SAFETY: 空提交（没有命令缓冲）只 signal 一个 timeline 值；timeline 由本结构持有。
        self.with_queue(|queue| unsafe {
            self.device
                .raw()
                .queue_submit(queue, &[submit], vk::Fence::null())
        })
        .map_err(|error| self.classify_failure(error, "queue_submit(idle)"))?;
        let wait = vk::SemaphoreWaitInfo::default()
            .semaphores(&timeline)
            .values(&values);
        // SAFETY: 等待的 timeline 由本结构持有；有界超时保证设备挂死时 UI 不会永久阻塞。
        match unsafe { self.device.raw().wait_semaphores(&wait, GPU_TIMEOUT_NS) } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => Err(self.mark_lost(
                smol_str::format_smolstr!(
                    "GPU idle wait timed out after {} ms (device still present; GPU hung)",
                    GPU_TIMEOUT_NS / 1_000_000
                ),
                0,
            )),
            Err(error) => Err(self.classify_failure(error, "wait_semaphores(idle)")),
        }
    }

    /// 有界等待一个 fence（图集初始布局 / 读回）。fence 由调用方创建并负责销毁。
    pub(crate) fn wait_fence(
        &self,
        fence: vk::Fence,
        context: &'static str,
    ) -> Result<(), EngineError> {
        let fences = [fence];
        // SAFETY: fence 由调用方创建，调用期间有效；有界等待。
        match unsafe { self.device.raw().wait_for_fences(&fences, true, GPU_TIMEOUT_NS) } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => Err(self.mark_lost(
                smol_str::format_smolstr!(
                    "{context}: GPU fence wait timed out after {} ms (device still present; GPU hung)",
                    GPU_TIMEOUT_NS / 1_000_000
                ),
                0,
            )),
            Err(error) => Err(self.classify_failure(error, context)),
        }
    }

    /// 图集读锁：持有期间页不会变（录制 + 提交一帧的整个区间）。
    pub(crate) fn atlas(&self) -> RwLockReadGuard<'_, SharedAtlas> {
        self.atlas.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// 共享图集当前页尺寸的无锁快照（上传入队时记录「入队页」）。
    pub(crate) fn atlas_page(&self, kind: AtlasKind) -> u32 {
        self.atlas_pages[kind.index() as usize].load(Ordering::Acquire)
    }

    /// 把一个或多个图集页重建到指定页尺寸。
    ///
    /// 写锁内重新检查（排队期间别的窗口可能已经建好同一页），然后等共享队列 idle：此后没有
    /// 任何窗口的 GPU 工作引用旧纹理 / 旧描述符。新纹理先转到 `SHADER_READ_ONLY_OPTIMAL`
    /// （之后所有窗口的增量上传都从该布局出发），最后重写全部窗口描述符段里的图集绑定。
    pub(crate) fn rebuild_atlas_pages(
        &self,
        requests: &[(AtlasKind, u32)],
    ) -> Result<AtlasRebuilt, EngineError> {
        let mut atlas = self.atlas.write().unwrap_or_else(PoisonError::into_inner);
        let pending: Vec<(AtlasKind, u32)> = requests
            .iter()
            .copied()
            .filter(|(kind, page)| atlas.page(*kind) != *page)
            .collect();
        let mut rebuilt = AtlasRebuilt::default();
        if pending.is_empty() {
            return Ok(rebuilt);
        }
        self.wait_queue_idle()?;
        for (kind, page) in &pending {
            atlas.recreate(&self.device, *kind, *page)?;
            self.atlas_pages[kind.index() as usize].store(*page, Ordering::Release);
            rebuilt.mark(*kind);
        }
        debug_assert!(
            rebuilt.glyph || rebuilt.image,
            "a non-empty rebuild must recreate at least one atlas page"
        );
        let kinds: Vec<AtlasKind> = pending.iter().map(|(kind, _)| *kind).collect();
        self.transition_atlas_layout(&atlas, &kinds)?;
        {
            let descriptors = self
                .descriptors
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.pipeline.write_atlas(
                descriptors.all_sets(),
                atlas.glyph_view(),
                atlas.image_view(),
                atlas.sampler(),
            );
        }
        Ok(rebuilt)
    }

    /// 给一个窗口分配共享描述符池里的段（`Drop` 时归还）。
    pub(crate) fn window_descriptors(self: &Arc<Self>) -> Result<DescriptorLease, EngineError> {
        let mut descriptors = self
            .descriptors
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let chunk = descriptors.allocate().ok_or_else(|| EngineError::AppInit {
            detail: smol_str::format_smolstr!(
                "descriptor pool exhausted (window segments: {MAX_WINDOWS})"
            ),
        })?;
        let sets = descriptors.chunk(chunk);
        Ok(DescriptorLease {
            gpu: Arc::clone(self),
            chunk,
            sets,
        })
    }

    fn release_window_descriptors(&self, chunk: usize) {
        self.descriptors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .free(chunk);
    }

    /// 把本帧的实例 / 裁剪缓冲写进该窗口帧槽的 descriptor set。
    ///
    /// 整个过程持共享描述符池锁：`vkUpdateDescriptorSets` 要求 descriptor pool 外部同步
    /// （别的窗口可能同时在写自己的段）。
    pub(crate) fn write_slot(
        &self,
        sets: &[vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT],
        slot: usize,
        upload: vk::Buffer,
        clip_offset: u64,
    ) {
        let _guard = self
            .descriptors
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.pipeline.write_slot(sets[slot], upload, clip_offset);
    }

    /// 登记一个资源，返回可放进 retire 队列的句柄。
    pub(crate) fn allocate(&self, object: GpuObject) -> GpuResourceHandle {
        self.resources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(object)
    }

    /// 释放句柄对应的资源；陈旧句柄返回 `false`。
    pub(crate) fn release(&self, handle: GpuResourceHandle) -> bool {
        let object = self
            .resources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take(handle);
        match object {
            Some(object) => {
                object.destroy(self.device.raw());
                true
            }
            None => false,
        }
    }

    /// 测试：共享设备句柄表里仍存活的资源数（关窗泄漏回归面）。
    #[cfg(test)]
    pub(crate) fn live_resources(&self) -> usize {
        self.resources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live()
    }

    /// 任一 Vulkan 调用失败后的分类：设备丢失 → 进程级粘性 `DeviceLost`（并落诊断）；否则
    /// `None`（调用方保留自己的错误分类）。
    pub(crate) fn note_failure(&self, result: vk::Result, frame: u64) -> Option<EngineError> {
        if let Some(error) = self.lost_error() {
            return Some(error);
        }
        if result != vk::Result::ERROR_DEVICE_LOST {
            return None;
        }
        Some(self.mark_lost(
            smol_str::format_smolstr!("VK_ERROR_DEVICE_LOST ({result:?})"),
            frame,
        ))
    }

    /// 记录粘性设备丢失（超时路径用：没有 `VkResult` 可分类，但 GPU 已经不可用）。
    pub(crate) fn mark_lost(&self, reason: SmolStr, frame: u64) -> EngineError {
        if let Some(error) = self.lost_error() {
            return error;
        }
        let log_path = write_diagnostics(&reason, frame);
        let mut lost = self.lost.lock().unwrap_or_else(PoisonError::into_inner);
        *lost = Some(LostInfo {
            reason: reason.clone(),
            log_path: log_path.clone(),
        });
        EngineError::DeviceLost { reason, log_path }
    }

    pub(crate) fn lost_error(&self) -> Option<EngineError> {
        let lost = self.lost.lock().unwrap_or_else(PoisonError::into_inner);
        lost.as_ref().map(|lost| EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        })
    }

    /// 把一次 Vulkan 调用错误转成后端错误（设备丢失优先，其余保留 `init_error` 分类）。
    fn classify_failure(&self, result: vk::Result, context: &'static str) -> EngineError {
        match self.note_failure(result, 0) {
            Some(lost) => lost,
            None => init_error(context, format!("{result:?}")),
        }
    }

    /// 新建图集页的布局初始化：`UNDEFINED → SHADER_READ_ONLY_OPTIMAL`。
    ///
    /// 调用方要么在进程初始化（还没有任何窗口），要么在页重建的图集写锁内（队列刚 idle），
    /// 因此这次临时提交不会与任何窗口命令交错。
    fn transition_atlas_layout(
        &self,
        atlas: &SharedAtlas,
        kinds: &[AtlasKind],
    ) -> Result<(), EngineError> {
        if kinds.is_empty() {
            return Ok(());
        }
        let raw = self.device.raw();
        // SAFETY: 临时命令池 / 缓冲 / fence 只在本函数内使用。
        let pool = unsafe {
            raw.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(self.queue_family())
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }
        .map_err(|error| {
            init_error(
                "create_command_pool(atlas transition)",
                format!("{error:?}"),
            )
        })?;
        let outcome = self.record_and_wait_transitions(raw, pool, atlas, kinds);
        // SAFETY: 队列已在 `record_and_wait_transitions` 内等到（成功或设备丢失均可销毁）。
        unsafe { raw.destroy_command_pool(pool, None) };
        outcome
    }

    fn record_and_wait_transitions(
        &self,
        raw: &ash::Device,
        pool: vk::CommandPool,
        atlas: &SharedAtlas,
        kinds: &[AtlasKind],
    ) -> Result<(), EngineError> {
        // SAFETY: 命令缓冲从本函数创建的池里分配，提交并等待之后释放。
        let command = unsafe {
            raw.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .map_err(|error| {
            init_error(
                "allocate_command_buffers(atlas transition)",
                format!("{error:?}"),
            )
        })?[0];
        // SAFETY: fence 由本函数创建并销毁；初始未 signal。
        let fence = unsafe { raw.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(|error| init_error("create_fence(atlas transition)", format!("{error:?}")))?;
        let outcome = (|| -> Result<(), EngineError> {
            // SAFETY: 命令缓冲由本函数独占，只录制一次。
            unsafe {
                raw.begin_command_buffer(
                    command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
            }
            .map_err(|error| {
                init_error(
                    "begin_command_buffer(atlas transition)",
                    format!("{error:?}"),
                )
            })?;
            atlas.record_layout_transitions(raw, command, kinds);
            // SAFETY: 录制收尾。
            unsafe { raw.end_command_buffer(command) }.map_err(|error| {
                init_error("end_command_buffer(atlas transition)", format!("{error:?}"))
            })?;
            let commands = [command];
            let submit = vk::SubmitInfo::default().command_buffers(&commands);
            // SAFETY: 命令已录制完毕；fence / 命令缓冲由本函数持有。
            self.with_queue(|queue| unsafe { raw.queue_submit(queue, &[submit], fence) })
                .map_err(|error| self.classify_failure(error, "queue_submit(atlas transition)"))?;
            self.wait_fence(fence, "wait_for_fences(atlas transition)")
        })();
        // SAFETY: fence / 命令缓冲归本函数独占（等待已在上面完成或设备已丢失）。
        unsafe {
            raw.destroy_fence(fence, None);
            raw.free_command_buffers(pool, &[command]);
        }
        outcome
    }
}

impl Drop for SharedGpu {
    fn drop(&mut self) {
        // 最后一个窗口关闭：正常路径每个窗口已经在自己的 drop 里注销了全部句柄（retire 队列
        // 残留 / 上传缓冲 / 几何缓冲 / 图集 staging）。这里兜底销毁错误路径（录制后失败、
        // device lost）遗留的句柄 —— 进程级设备的句柄表不注销就会泄漏到进程退出。
        let raw = self.device.raw().clone();
        let resources = self
            .resources
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        resources.drain_all(|object| object.destroy(&raw));
        // SAFETY: idle timeline 只属于本结构；队列上的工作已由最后窗口的 teardown 等到。
        unsafe { raw.destroy_semaphore(self.idle_timeline, None) };
    }
}

/// 一个窗口在共享描述符池里的段（`Drop` 时把段归还）。
///
/// 窗口渲染器在 GPU 线程 join（或同步渲染完成）之后才销毁，因此归还时该窗口的描述符不再被
/// 任何在飞命令引用；新窗口拿到同一段后重写描述符是安全的。
pub(crate) struct DescriptorLease {
    gpu: Arc<SharedGpu>,
    chunk: usize,
    sets: [vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT],
}

impl DescriptorLease {
    /// 本窗口的 `MAX_FRAMES_IN_FLIGHT` 个 descriptor set（按帧槽索引）。
    pub(crate) fn sets(&self) -> &[vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT] {
        &self.sets
    }
}

impl Drop for DescriptorLease {
    fn drop(&mut self) {
        self.gpu.release_window_descriptors(self.chunk);
    }
}

/// 每窗口的帧同步原语：每帧槽 fence、`image_available` 二值信号量、retire timeline（§7.2）。
///
/// 这些**不能**进程级共享：timeline 的 signal 值必须按每个窗口自己的帧序号单调递增；fence /
/// `image_available` 与「某个窗口的第 N 帧」绑定。`VkFence` / `VkSemaphore` 的所有权在这里，
/// 共享的只有它们背后的 `VkDevice` / queue。
pub(crate) struct FrameSync {
    shared: Arc<SharedGpu>,
    /// retire 记账用的 timeline（每帧 signal 一个值；绝不用于 acquire/present）。
    timeline: vk::Semaphore,
    /// 每帧槽一个（`InFlightRing` 记账，复用命令/上传缓冲的前置条件）。
    fences: [vk::Fence; MAX_FRAMES_IN_FLIGHT],
    /// acquire 的 `image_available`（每帧槽一个，§7.2）：跨 swapchain 重建存活。
    image_available: [vk::Semaphore; MAX_FRAMES_IN_FLIGHT],
    ring: InFlightRing,
    retire: RetireQueue,
}

impl FrameSync {
    pub(crate) fn new(shared: &Arc<SharedGpu>) -> Result<Self, EngineError> {
        let raw = shared.raw();
        let mut timeline_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        // SAFETY: `timeline_info` 是栈上值，只在本次创建期间被读。
        let timeline = unsafe {
            raw.create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut timeline_info),
                None,
            )
        }
        .map_err(|error| init_error("create_semaphore(timeline)", format!("{error:?}")))?;

        let mut fences = [vk::Fence::null(); MAX_FRAMES_IN_FLIGHT];
        for fence in fences.iter_mut() {
            // 必须带 `SIGNALED`：槽位复用时先等 fence，首帧（以及早退后未提交的槽）不能永远等下去。
            // SAFETY: 句柄归本结构独占；`Drop` 里销毁。
            *fence = unsafe {
                raw.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
            }
            .map_err(|error| init_error("create_fence", format!("{error:?}")))?;
        }
        // 每帧槽一个二值信号量（§7.2）：acquire 时 signal、submit 时在 TRANSFER 等它
        // （swapchain image 的首次使用是 layout transition + 整面 copy，F-168）。
        let mut image_available = [vk::Semaphore::null(); MAX_FRAMES_IN_FLIGHT];
        for semaphore in image_available.iter_mut() {
            // SAFETY: 句柄归本结构独占；`Drop` 里销毁。
            *semaphore = unsafe { raw.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
                .map_err(|error| {
                    init_error("create_semaphore(image_available)", format!("{error:?}"))
                })?;
        }
        Ok(Self {
            shared: Arc::clone(shared),
            timeline,
            fences,
            image_available,
            ring: InFlightRing::new(),
            retire: RetireQueue::new(),
        })
    }

    /// 取本帧的 ring 槽位（**纯记账**，不碰 GPU；提交成功后才 [`FrameSync::advance`]）。
    ///
    /// 槽位按 **acquire 次数**轮转（0 → 1 → 2 → 0），与三槽 fence / `image_available` 同源；
    /// 不能由请求序号取模决定（最新帧槽合并会丢掉被覆盖帧的序号，见
    /// [`crate::render::frame::InFlightRing::acquire`]）。
    ///
    /// 同一帧可能因 `NOT_READY` / `OUT_OF_DATE` 重试多次：每次重试会轮到下一个槽，这不影响
    /// 正确性（每槽在 acquire 之前都等自己的 fence），`signal_value` 仍随提交单调递增。
    pub(crate) fn slot(&mut self) -> FrameSlot {
        let slot = self.ring.acquire();
        debug_assert_eq!(
            self.ring.last_signaled(),
            slot.signal_value.saturating_sub(1),
            "ring must advance exactly one step per submitted frame"
        );
        slot
    }

    /// 本帧槽的 `image_available` 二值信号量（§7.2：每帧槽一个）。
    pub(crate) fn image_available(&self, slot: usize) -> vk::Semaphore {
        self.image_available[slot]
    }

    /// 等本槽上一次提交完成（**允许阻塞**：只发生在 GPU 线程，§8.2）。
    ///
    /// fence 在创建时带 `SIGNALED`，且只在 `reset_fence`（提交前）被重置 ⇒ 早退
    /// （`NOT_READY` / `OUT_OF_DATE` / Geometry 注册失败）不会把槽卡死。
    pub(crate) fn wait_slot(&mut self, slot: FrameSlot) -> Result<(), EngineError> {
        let fences = [self.fences[slot.index]];
        // SAFETY: fence 由本结构创建；`u64::MAX` = 等到它 signal（该槽上一次提交完成）。
        match unsafe { self.shared.raw().wait_for_fences(&fences, true, u64::MAX) } {
            Ok(()) => Ok(()),
            // §7.7：设备丢失优先分类成粘性 `DeviceLost`，不降级成泛化的 `AppInit`。
            Err(error) => match self.shared.note_failure(error, 0) {
                Some(lost) => Err(lost),
                None => Err(init_error("wait_for_fences", format!("{error:?}"))),
            },
        }
    }

    /// 提交前重置本槽 fence（唯一的 reset 点：`queue_submit` 会立刻让它 signal）。
    pub(crate) fn reset_fence(&self, slot: usize) -> Result<(), EngineError> {
        let fences = [self.fences[slot]];
        // SAFETY: 句柄由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        unsafe { self.shared.raw().reset_fences(&fences) }
            .map_err(|error| init_error("reset_fences", format!("{error:?}")))
    }

    pub(crate) fn fence(&self, slot: usize) -> vk::Fence {
        self.fences[slot]
    }

    pub(crate) fn advance(&mut self, slot: FrameSlot) {
        self.ring.advance(slot);
    }

    /// 非阻塞读 timeline 的完成值（retire 记账，§4.3 / I4）。
    ///
    /// timeline **只用于 retire**：`acquire` / `present` 一律用二值信号量（§7.2 硬规则 2），
    /// 帧的完成也一律用 fence 观察；这里没有 `wait_semaphores`。
    pub(crate) fn completed_value(&self) -> Result<u64, EngineError> {
        // SAFETY: timeline semaphore 由本结构创建，只读查询。
        unsafe { self.shared.raw().get_semaphore_counter_value(self.timeline) }
            .map_err(|error| init_error("get_semaphore_counter_value", format!("{error:?}")))
    }

    pub(crate) fn timeline(&self) -> vk::Semaphore {
        self.timeline
    }

    /// 最靠后的已提交 timeline 值（teardown 的等待点）。
    pub(crate) fn submitted_value(&self) -> u64 {
        self.ring.last_signaled()
    }

    /// 等本窗口自己的最后一次提交完成（teardown 前，**有界**）。
    ///
    /// 只等本窗口的 timeline，不是 `vkDeviceWaitIdle`：共享设备上别的窗口可能还有在飞工作，
    /// 那些与本窗口的资源无关（本窗口的 swapchain / persist / 命令缓冲 / 描述符段都不共享）。
    pub(crate) fn wait_idle(&self) -> Result<(), EngineError> {
        let value = self.submitted_value();
        if value == 0 {
            return Ok(());
        }
        let timeline = [self.timeline];
        let values = [value];
        let wait = vk::SemaphoreWaitInfo::default()
            .semaphores(&timeline)
            .values(&values);
        // SAFETY: timeline 由本结构持有；有界超时保证设备挂死时窗口关闭不会永久阻塞。
        match unsafe { self.shared.raw().wait_semaphores(&wait, GPU_TIMEOUT_NS) } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => Err(self.shared.mark_lost(
                smol_str::format_smolstr!(
                    "GPU teardown wait timed out after {} ms (device still present; GPU hung)",
                    GPU_TIMEOUT_NS / 1_000_000
                ),
                0,
            )),
            Err(error) => match self.shared.note_failure(error, 0) {
                Some(lost) => Err(lost),
                None => Err(init_error(
                    "wait_semaphores(teardown)",
                    format!("{error:?}"),
                )),
            },
        }
    }

    /// 延迟销毁：本帧用过的资源至少要活到 `index` 的 fence 完成。
    pub(crate) fn retire(&mut self, index: FrameIndex, handle: GpuResourceHandle) {
        self.retire.retire(index, handle);
    }

    /// **非阻塞**推进 retire：timeline 已到达的帧（含被跳过的序号，§4.3）及更早的句柄交回
    /// 共享设备真正释放。返回释放数。
    pub(crate) fn drain_retire(&mut self, completed: FrameIndex) -> usize {
        let shared = Arc::clone(&self.shared);
        self.retire.on_completed(completed, &mut |handle| {
            let _ = shared.release(handle);
        })
    }

    /// teardown：把 retire 队列里**全部**残留句柄交回共享设备 —— 包括录制失败、从未提交的帧
    /// （正常路径不会观察到它们的完成）。调用方保证本窗口的 GPU 工作已完成。
    pub(crate) fn drain_all_retired(&mut self) -> usize {
        let shared = Arc::clone(&self.shared);
        self.retire.drain_all(&mut |handle| {
            let _ = shared.release(handle);
        })
    }

    /// 尚未释放的 retire 句柄数（测试：teardown / 扩容路径）。
    #[cfg(test)]
    pub(crate) fn pending_retired(&self) -> usize {
        self.retire.pending()
    }
}

impl Drop for FrameSync {
    fn drop(&mut self) {
        // SAFETY: 句柄归本结构独占；窗口 teardown 已等到本窗口最后一次提交完成。
        unsafe {
            for fence in self.fences {
                self.shared.raw().destroy_fence(fence, None);
            }
            for semaphore in self.image_available {
                self.shared.raw().destroy_semaphore(semaphore, None);
            }
            self.shared.raw().destroy_semaphore(self.timeline, None);
        }
    }
}

/// 资源句柄表（generation 拒绝陈旧句柄）。
struct ResourceTable {
    slots: Vec<Option<GpuObject>>,
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

    fn insert(&mut self, object: GpuObject) -> GpuResourceHandle {
        if let Some(index) = self.free.pop() {
            let generation = self.generations[index as usize].wrapping_add(1);
            self.generations[index as usize] = generation;
            self.slots[index as usize] = Some(object);
            GpuResourceHandle::new(HANDLE_KIND_OBJECT, generation, index)
        } else {
            let index = self.slots.len() as u32;
            self.slots.push(Some(object));
            self.generations.push(0);
            GpuResourceHandle::new(HANDLE_KIND_OBJECT, 0, index)
        }
    }

    fn take(&mut self, handle: GpuResourceHandle) -> Option<GpuObject> {
        if handle.kind() != HANDLE_KIND_OBJECT {
            return None;
        }
        let index = handle.index() as usize;
        if self.generations.get(index).copied() != Some(handle.generation()) {
            return None;
        }
        let object = self.slots.get_mut(index)?.take();
        if object.is_some() {
            self.free.push(index as u32);
        }
        object
    }

    #[cfg(test)]
    fn live(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// teardown 兜底：销毁全部残留句柄。
    fn drain_all(&mut self, mut destroy: impl FnMut(GpuObject)) {
        for slot in &mut self.slots {
            if let Some(object) = slot.take() {
                destroy(object);
            }
        }
        self.free.clear();
    }
}

/// `AppInit` 的 `detail`。
pub(crate) fn init_error(context: &'static str, detail: impl std::fmt::Display) -> EngineError {
    EngineError::AppInit {
        detail: smol_str::format_smolstr!("vulkan {context}: {detail}"),
    }
}

/// 设备丢失诊断文件（`%TEMP%/kytrix-vk-<pid>-<frame>.txt`）。
fn write_diagnostics(reason: &str, frame: u64) -> PathBuf {
    let path = std::env::temp_dir().join(format!("kytrix-vk-{}-{frame}.txt", std::process::id()));
    let body = format!("kytrix vulkan device lost\nreason: {reason}\n");
    let _ = std::fs::write(&path, body);
    path
}

/// GPU 测试的进程级串行锁：多个独立实例 / 多个 GPU 线程并行会互相拖垮（lavapipe 尤甚）。
#[cfg(test)]
pub(crate) fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::{GpuObject, ResourceTable, SharedGpu, gpu_test_lock};
    use crate::paint::atlas::AtlasKind;

    /// 引用计数单例：两次取到的是同一个 `SharedGpu`（实例 / 设备 / queue / PSO / 图集只建一次）。
    #[test]
    fn the_process_wide_gpu_is_created_once() {
        let _guard = gpu_test_lock();
        let first = SharedGpu::process_headless().expect("headless shared gpu");
        let second = SharedGpu::process_headless().expect("headless shared gpu");
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(
            first.atlas().page(AtlasKind::Glyph),
            second.atlas().page(AtlasKind::Glyph)
        );
    }

    /// 共享图集的页换代：写锁 + 有界队列 idle 之后替换纹理，页尺寸与描述符一起前进。
    #[test]
    fn rebuilding_a_shared_atlas_page_swaps_the_texture() {
        let _guard = gpu_test_lock();
        let shared = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 1, "占位页是 1×1");
        shared
            .rebuild_atlas_pages(&[(AtlasKind::Glyph, 64)])
            .expect("rebuild");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 64);
        assert_eq!(
            shared.atlas_page(AtlasKind::Glyph),
            64,
            "无锁快照与写锁内页尺寸必须一致"
        );
        // 同一页重复请求是 no-op（其余窗口在写锁释放后不必再重建）。
        shared
            .rebuild_atlas_pages(&[(AtlasKind::Glyph, 64)])
            .expect("no-op rebuild");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 64);
    }

    /// 每个窗口在共享描述符池里拿到互不重叠的段；段归还后可以被下一个窗口复用。
    #[test]
    fn windows_receive_disjoint_and_reusable_descriptor_segments() {
        let _guard = gpu_test_lock();
        let shared = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let first = shared.window_descriptors().expect("first segment");
        let second = shared.window_descriptors().expect("second segment");
        for slot in 0..first.sets().len() {
            assert_ne!(
                first.sets()[slot],
                second.sets()[slot],
                "两个窗口的帧槽描述符不得共享"
            );
        }
        let recycled = first.sets()[0];
        drop(first);
        let third = shared.window_descriptors().expect("recycled segment");
        assert_eq!(
            third.sets()[0],
            recycled,
            "归还的段必须被复用（池容量有界）"
        );
    }

    /// 陈旧句柄被 generation 拒绝；teardown 的 `drain_all` 把残留对象真正交出来。
    #[test]
    fn resource_handles_reject_stale_generations_and_drain_on_teardown() {
        let mut table = ResourceTable::new();
        let handle = table.insert(GpuObject::Buffer(vk::Buffer::null()));
        assert!(table.take(handle).is_some());
        assert!(table.take(handle).is_none(), "double take is rejected");

        let recycled = table.insert(GpuObject::Buffer(vk::Buffer::null()));
        assert_eq!(recycled.index(), handle.index(), "slot is reused");
        assert_ne!(recycled.generation(), handle.generation());
        assert!(
            table.take(handle).is_none(),
            "stale generation must be rejected"
        );

        table.insert(GpuObject::Memory(vk::DeviceMemory::null()));
        let mut drained = 0usize;
        table.drain_all(|_| drained += 1);
        assert_eq!(drained, 2, "teardown drains every remaining object");
        assert_eq!(table.live(), 0);
    }
}

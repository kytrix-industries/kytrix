//! 物理设备选择、D8 采集、逻辑设备与同步原语（`design/phase-12-macos-linux.md` §6.2、
//! `design/async-render-pipeline.md` §7.2）。
//!
//! 同步原语的所有权（§7.2 的两条硬规则）：
//! - `VkFence` 每帧槽一个（`InFlightRing` 记账，复用命令/上传缓冲的前置条件）；
//! - `image_available` 二值信号量**每帧槽一个**（acquire 时 signal、submit 时 wait）；
//! - `render_finished` 二值信号量**每 swapchain image 一个**（在 `swapchain.rs`，按 image 索引）；
//! - timeline 信号量 1 个：每帧 signal 一个值，**只用于 retire 记账**，绝不用于 acquire/present。

use std::{cell::Cell, path::PathBuf};

use ash::vk;
use smol_str::SmolStr;

use super::instance::VulkanInstance;
use crate::{
    error::EngineError,
    render::{
        frame::{FrameSlot, InFlightRing, MAX_FRAMES_IN_FLIGHT},
        retire::{GpuResourceHandle, RetireQueue},
    },
    window::FrameIndex,
};

/// D8 硬基线（错误文案与安装器共用的唯一来源）。
pub(crate) const D8_REQUIRED: &str = "Vulkan 1.2 / descriptorIndexing / timelineSemaphore / VK_KHR_swapchain / VK_KHR_wayland_surface";

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
    /// `VK_KHR_wayland_surface`（实例创建时强制；记录在这里供诊断与安装器共用）。
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
    static ON_GPU_THREAD: Cell<bool> = const { Cell::new(false) };
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
pub(crate) enum GpuObject {
    Buffer(vk::Buffer),
    Memory(vk::DeviceMemory),
}

/// 一个逻辑设备 + 队列 + 同步原语 + 资源句柄表。
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
    timeline: vk::Semaphore,
    fences: [vk::Fence; MAX_FRAMES_IN_FLIGHT],
    /// acquire 的 `image_available`（每帧槽一个，§7.2）：跨 swapchain 重建存活。
    image_available: [vk::Semaphore; MAX_FRAMES_IN_FLIGHT],
    ring: InFlightRing,
    retire: RetireQueue,
    resources: ResourceTable,
    lost: Option<LostInfo>,
}

/// 设备丢失的粘性记录。
struct LostInfo {
    reason: SmolStr,
    log_path: PathBuf,
}

impl Device {
    /// 选卡（有 Wayland surface 支持的第一个）→ 采 features/扩展 → 建 device → 校验 D8。
    pub(crate) fn new(instance: &VulkanInstance) -> Result<Self, EngineError> {
        // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
        let devices = unsafe { instance.raw().enumerate_physical_devices() }
            .map_err(|error| init_error("enumerate_physical_devices", format!("{error:?}")))?;
        let surface_fn = instance.surface_fn();
        let mut chosen = None;
        for physical in devices {
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
            // 离屏实例没有 surface：只要求图形队列族（`surface_fn` 为 `None`）。
            let supported = match surface_fn {
                Some(surface_fn) => {
                    // SAFETY: `family` 来自本设备的队列族列表；surface 由同一实例创建。
                    unsafe {
                        surface_fn.get_physical_device_surface_support(
                            physical,
                            family as u32,
                            instance.surface(),
                        )
                    }
                    .unwrap_or(false)
                }
                None => true,
            };
            if supported {
                chosen = Some((physical, family as u32));
                break;
            }
        }
        let (physical, queue_family) = chosen.ok_or(EngineError::UnsupportedGpu {
            os: "linux",
            required: D8_REQUIRED,
            actual: SmolStr::new_static("no device with a Wayland surface queue"),
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
            // 实例创建时已强制 `VK_KHR_wayland_surface`（`VulkanInstance::new`），否则到不了这里。
            wayland_surface: true,
            incremental_present,
        };
        caps.check()?;
        // 离屏实例（快照）没有 surface，自然没有 swapchain：那条路径不需要 present。
        if surface_fn.is_some() && !caps.swapchain {
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
        let enable_swapchain = caps.swapchain && surface_fn.is_some();
        let mut extension_pointers = Vec::with_capacity(2);
        if enable_swapchain {
            extension_pointers.push(ash::khr::swapchain::NAME.as_ptr());
        }
        if incremental_present && enable_swapchain {
            extension_pointers.push(ash::khr::incremental_present::NAME.as_ptr());
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
            // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
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
        // 它只属于帧槽（不属于 swapchain），所以重建 swapchain 时保留、无需重建。
        let mut image_available = [vk::Semaphore::null(); MAX_FRAMES_IN_FLIGHT];
        for semaphore in image_available.iter_mut() {
            // SAFETY: 句柄与参数都由本结构独占；`Drop` 里销毁。
            *semaphore = unsafe { raw.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
                .map_err(|error| {
                    init_error("create_semaphore(image_available)", format!("{error:?}"))
                })?;
        }

        Ok(Self {
            raw,
            physical,
            queue,
            queue_family,
            caps,
            memory_properties,
            timestamp_period: properties.limits.timestamp_period,
            timestamp_valid_bits,
            timeline,
            fences,
            image_available,
            ring: InFlightRing::new(),
            retire: RetireQueue::new(),
            resources: ResourceTable::new(),
            lost: None,
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

    /// 取本帧的 ring 槽位（**纯记账**，不碰 GPU；提交成功后才 [`Device::advance`]）。
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
        // SAFETY: fence 由本设备创建；`u64::MAX` = 等到它 signal（该槽上一次提交完成）。
        match unsafe { self.raw.wait_for_fences(&fences, true, u64::MAX) } {
            Ok(()) => Ok(()),
            // §7.7：设备丢失优先分类成粘性 `DeviceLost`，不降级成泛化的 `AppInit`。
            Err(error) => {
                if let Some(lost) = self.note_failure(error, 0) {
                    return Err(lost);
                }
                Err(init_error("wait_for_fences", format!("{error:?}")))
            }
        }
    }

    /// 提交前重置本槽 fence（唯一的 reset 点：`queue_submit` 会立刻让它 signal）。
    pub(crate) fn reset_fence(&self, slot: usize) -> Result<(), EngineError> {
        let fences = [self.fences[slot]];
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        unsafe { self.raw.reset_fences(&fences) }
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
        // SAFETY: timeline semaphore 由本设备创建，只读查询。
        unsafe { self.raw.get_semaphore_counter_value(self.timeline) }
            .map_err(|error| init_error("get_semaphore_counter_value", format!("{error:?}")))
    }

    pub(crate) fn timeline(&self) -> vk::Semaphore {
        self.timeline
    }

    /// device lost 的分类（粘性）。
    pub(crate) fn note_failure(&mut self, result: vk::Result, frame: u64) -> Option<EngineError> {
        if self.lost.is_some() {
            return self.lost_error();
        }
        if result != vk::Result::ERROR_DEVICE_LOST {
            return None;
        }
        let reason = smol_str::format_smolstr!("VK_ERROR_DEVICE_LOST ({result:?})");
        let log_path = write_diagnostics(&reason, frame);
        let lost = LostInfo {
            reason: reason.clone(),
            log_path: log_path.clone(),
        };
        self.lost = Some(lost);
        Some(EngineError::DeviceLost { reason, log_path })
    }

    pub(crate) fn lost_error(&self) -> Option<EngineError> {
        self.lost.as_ref().map(|lost| EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        })
    }

    pub(crate) fn allocate(&mut self, object: GpuObject) -> GpuResourceHandle {
        self.resources.insert(object)
    }

    /// 把 `index` 及更早的句柄真正销毁（fence 已到达之后调用）。
    pub(crate) fn drain_retire(&mut self, index: FrameIndex) -> usize {
        let raw = self.raw.clone();
        let mut destroyed = 0;
        self.retire.on_completed(index, &mut |handle| {
            if let Some(object) = self.resources.take(handle) {
                object.destroy(&raw);
                destroyed += 1;
            }
        });
        destroyed
    }

    /// 尚未释放的 retire 句柄数（测试：一次性 staging 的生命周期）。
    #[cfg(test)]
    pub(crate) fn pending_retired(&self) -> usize {
        self.retire.pending()
    }

    pub(crate) fn retire(&mut self, index: FrameIndex, handle: GpuResourceHandle) {
        self.retire.retire(index, handle);
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: 所有子资源必须由调用方先销毁（`GpuRenderer::drop` 保证顺序）。
        unsafe {
            for fence in self.fences {
                self.raw.destroy_fence(fence, None);
            }
            for semaphore in self.image_available {
                self.raw.destroy_semaphore(semaphore, None);
            }
            self.raw.destroy_semaphore(self.timeline, None);
            self.raw.destroy_device(None);
        }
    }
}

impl GpuObject {
    /// 销毁底层 Vulkan 对象（`Device::release` / `drain_retire` 的唯一出口）。
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

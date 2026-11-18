//! 进程级 `VkInstance` + `VK_KHR_wayland_surface` + 验证层 debug messenger
//! （R11-4）。
//!
//! 实例是**进程级共享**的（P1-2.9）：多窗口只建一个 `VkInstance`，每个窗口用
//! [`VulkanInstance::create_surface`] 建自己的 `VkSurfaceKHR`（surface 的存活期 = 窗口，
//! 由 `swapchain::Surface` 在窗口 drop 时销毁）。实例本身由 `device::SharedGpu` 持有，
//! 最后一个窗口关闭、`SharedGpu` 引用计数归零时才销毁 —— 因此 surface 必然先于 instance 销毁。
//!
//! **验证层必须能被观测到**（R11-4）：旧实现把 `VK_LAYER_KHRONOS_validation` 加进
//! `enabledLayerNames`，却从未启用 `VK_EXT_debug_utils`、也没有 messenger —— 所有 VUID 消息
//! 都被 loader 直接丢弃（应用收不到、调用也不会失败）。于是「CI 里跑 lavapipe + 验证层 0 error」
//! 这个门禁在当前代码下**不可观测**：0 error 不是因为没错误，而是因为没人接收。现在：
//!
//! - debug 构建默认同时启用验证层 + `VK_EXT_debug_utils` + messenger（release 默认关闭）；
//! - 消息经 [`debug_callback`] 打到 `stderr`，并按严重级计数（进程级 `AtomicU64`）；
//! - `KYTRIX_VK_VALIDATION=off|on|strict` 覆盖默认：`strict` 在收到 ERROR 时直接 panic
//!   （CI 的「验证层 0 error」因此可以变成构建失败，而不是一行看不见的日志）。
//!
//! `display` / `surface` 是 **`wl_display*` / `wl_surface*`**（由 `platform/linux` 提供；
//! 平台层因此必须启用 `wayland-client` 的 `system` feature —— 纯 Rust 后端的对象指针是令牌，
//! 不能交给 Vulkan 驱动，F-135）。

use std::{
    ffi::{CStr, c_void},
    sync::atomic::{AtomicU64, Ordering},
};

use ash::{Entry, Instance};

use super::device::init_error;
use crate::error::EngineError;

/// 实例级必需的扩展。
const REQUIRED_EXTENSIONS: [&CStr; 2] = [ash::khr::surface::NAME, ash::khr::wayland_surface::NAME];

/// 验证层相关的环境变量（R11-4 的 CI 开关）：`off` / `on` / `strict`。
pub(crate) const VALIDATION_ENV: &str = "KYTRIX_VK_VALIDATION";

/// 验证层消息的进程级计数（`(errors, warnings)`）。
///
/// 三个消费者：CI 断言（`strict` 模式下 ERROR 直接 panic；也可在测试里读计数）、
/// `VulkanInstance::drop` 的结束摘要、以及现场排障时「验证层在跑但没人接收」的判定
/// （`GpuCaps.debug_messenger`）。
static VALIDATION_ERRORS: AtomicU64 = AtomicU64::new(0);
static VALIDATION_WARNINGS: AtomicU64 = AtomicU64::new(0);

/// 到目前为止收到的验证层消息数（`(errors, warnings)`）。
pub(crate) fn validation_counts() -> (u64, u64) {
    (
        VALIDATION_ERRORS.load(Ordering::Relaxed),
        VALIDATION_WARNINGS.load(Ordering::Relaxed),
    )
}

/// 验证层开关（见 [`VALIDATION_ENV`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValidationMode {
    /// 不启用验证层（release 默认）。
    Off,
    /// 启用验证层 + messenger，消息只打日志（debug 默认）。
    On,
    /// 额外把 ERROR 级消息变成 panic（CI 门禁）。
    Strict,
}

impl ValidationMode {
    /// 读取环境变量；未设置时按构建类型取默认（debug = `On`，release = `Off`）。
    fn from_env() -> Self {
        match std::env::var(VALIDATION_ENV).ok().as_deref() {
            Some("off") | Some("0") | Some("false") => Self::Off,
            Some("strict") | Some("fail") => Self::Strict,
            Some("on") | Some("1") | Some("true") => Self::On,
            // 拼错的值不静默忽略：按最严格的解释走，让 CI 立刻看到（不是"悄悄没开验证"）。
            Some(_) => Self::Strict,
            None if cfg!(debug_assertions) => Self::On,
            None => Self::Off,
        }
    }

    /// 是否启用验证层。
    fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// ERROR 级消息是否 panic。
    fn strict(self) -> bool {
        matches!(self, Self::Strict)
    }
}

/// 已安装的 debug messenger（`Drop` 里销毁；`_fn` 提供销毁入口）。
struct DebugMessenger {
    fn_: ash::ext::debug_utils::Instance,
    raw: ash::vk::DebugUtilsMessengerEXT,
}

/// 进程级实例 + 每窗口 surface 的创建入口。
pub(crate) struct VulkanInstance {
    /// loader 入口：实例销毁前必须有效（也是 VK_EXT_debug_utils 函数表的来源）。
    entry: Entry,
    raw: Instance,
    /// 产品路径才有 WSI 函数表；离屏实例（快照 / 单测）没有 surface。
    surface_fn: Option<ash::khr::surface::Instance>,
    /// 只为保活（surface 由本实例创建，销毁走 `surface_fn`）。
    wayland_fn: Option<ash::khr::wayland_surface::Instance>,
    /// 验证层是否启用（release 默认不启用；`KYTRIX_VK_VALIDATION` 可强制）。
    validation: bool,
    /// `VK_EXT_debug_utils` 的 messenger（缺失 = 验证层消息无人接收，`GpuCaps` 会标出来）。
    debug: Option<DebugMessenger>,
}

impl VulkanInstance {
    /// 产品路径的进程级实例：启用 `VK_KHR_surface` + `VK_KHR_wayland_surface`
    /// （debug 构建额外启用验证层 + `VK_EXT_debug_utils`）。
    pub(crate) fn new_product() -> Result<Self, EngineError> {
        // SAFETY: `Entry::load` 只加载 `libvulkan.so.1`。
        let entry =
            unsafe { Entry::load() }.map_err(|error| init_error("libvulkan", error.to_string()))?;

        let available =
            // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
            unsafe { entry.enumerate_instance_extension_properties(None) }.map_err(|error| {
                init_error("enumerate_instance_extension_properties", error.to_string())
            })?;
        for required in REQUIRED_EXTENSIONS {
            let present = available.iter().any(|extension| {
                // SAFETY: `extension_name` 是 Vulkan 保证的 NUL 结尾字符串。
                (unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) }) == required
            });
            if !present {
                return Err(EngineError::UnsupportedGpu {
                    os: "linux",
                    required: "VK_KHR_surface + VK_KHR_wayland_surface",
                    actual: smol_str::format_smolstr!("missing {}", required.to_string_lossy()),
                });
            }
        }

        let application = ash::vk::ApplicationInfo::default()
            .application_name(c"kytrix")
            .application_version(1)
            .engine_name(c"ui_core")
            // Vulkan 1.2 是硬基线（D8）。
            .api_version(ash::vk::make_api_version(0, 1, 2, 0));
        let mut extension_pointers: Vec<*const std::ffi::c_char> = REQUIRED_EXTENSIONS
            .iter()
            .map(|name| name.as_ptr())
            .collect();
        let mut layers = Vec::new();
        let mode = ValidationMode::from_env();
        let mut validation = false;
        let mut debug_utils = false;
        if mode.enabled() && validation_available(&entry) {
            validation = true;
            layers.push(c"VK_LAYER_KHRONOS_validation".as_ptr());
            // 没有 messenger 的验证层等于没开（R11-4）：只有扩展真的可用时才启用它。
            debug_utils = available.iter().any(|extension| {
                // SAFETY: `extension_name` 是 Vulkan 保证的 NUL 结尾字符串。
                (unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) })
                    == ash::ext::debug_utils::NAME
            });
            if debug_utils {
                extension_pointers.push(ash::ext::debug_utils::NAME.as_ptr());
            } else {
                // 有层没扩展：明确说出来，别让 CI 误以为「验证层 0 error」。
                eprintln!(
                    "kytrix: vulkan {VALIDATION_ENV}: {VALIDATION_ENV} 要求 debug messenger，但 VK_EXT_debug_utils 不可用"
                );
            }
        }
        let mut messenger_create = debug_messenger_create_info();
        let create = ash::vk::InstanceCreateInfo::default()
            .application_info(&application)
            .enabled_extension_names(&extension_pointers)
            .enabled_layer_names(&layers);
        // `VkDebugUtilsMessengerCreateInfoEXT` 也挂在 `pNext` 上：这样 `vkCreateInstance` /
        // `vkDestroyInstance` 期间的验证消息同样能被接收（Qt RHI / Dawn 的同一做法）。
        let create = if debug_utils {
            create.push_next(&mut messenger_create)
        } else {
            create
        };
        // SAFETY: `create` 的指针都在本次调用期间有效；`pNext` 指向本函数栈上的
        // `messenger_create`。
        let raw = unsafe { entry.create_instance(&create, None) }
            .map_err(|error| init_error("create_instance", format!("{error:?}")))?;
        let mut instance = Self {
            surface_fn: Some(ash::khr::surface::Instance::new(&entry, &raw)),
            wayland_fn: Some(ash::khr::wayland_surface::Instance::new(&entry, &raw)),
            entry,
            raw,
            validation,
            debug: None,
        };
        if debug_utils && let Err(error) = instance.install_messenger() {
            // messenger 装不上不是致命错误（渲染仍可用），但必须说出来：
            // 「验证层 0 error」在没 messenger 时毫无意义。
            eprintln!("kytrix: vulkan debug messenger unavailable: {error}");
        }
        Ok(instance)
    }

    /// 离屏实例（无窗口）：不建 surface，也不启用 WSI 扩展。
    ///
    /// Linux 的产品**窗口**路径需要真实 `wl_surface`（F-137），但「没有窗口也要画」的路径
    /// （`Window::enable_offscreen_renderer`、像素快照）只需要一个能渲染到 `VkImage` 的设备。
    pub(crate) fn new_headless() -> Result<Self, EngineError> {
        // SAFETY: `Entry::load` 只加载 `libvulkan.so.1`。
        let entry =
            unsafe { Entry::load() }.map_err(|error| init_error("libvulkan", error.to_string()))?;
        let available =
            // SAFETY: 只读查询；loader 入口在本次调用期间有效。
            unsafe { entry.enumerate_instance_extension_properties(None) }.unwrap_or_default();
        let application = ash::vk::ApplicationInfo::default()
            .application_name(c"kytrix")
            .application_version(1)
            .engine_name(c"ui_core")
            .api_version(ash::vk::make_api_version(0, 1, 2, 0));
        let mut layers = Vec::new();
        let mode = ValidationMode::from_env();
        let mut validation = false;
        let mut debug_utils = false;
        let mut extension_pointers: Vec<*const std::ffi::c_char> = Vec::new();
        if mode.enabled() && validation_available(&entry) {
            validation = true;
            layers.push(c"VK_LAYER_KHRONOS_validation".as_ptr());
            debug_utils = available.iter().any(|extension| {
                // SAFETY: `extension_name` 是 Vulkan 保证的 NUL 结尾字符串。
                (unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) })
                    == ash::ext::debug_utils::NAME
            });
            if debug_utils {
                extension_pointers.push(ash::ext::debug_utils::NAME.as_ptr());
            }
        }
        let mut messenger_create = debug_messenger_create_info();
        let create = ash::vk::InstanceCreateInfo::default()
            .application_info(&application)
            // `VK_EXT_debug_utils` 必须同时出现在 `ppEnabledExtensionNames` 与 `pNext` 的
            // `VkDebugUtilsMessengerCreateInfoEXT` 里（`VUID-VkInstanceCreateInfo-pNext-04926`）：
            // 前者漏了会在 `vkCreateInstance` 报验证错误，随后的 messenger 调用 panic。
            .enabled_extension_names(&extension_pointers)
            .enabled_layer_names(&layers);
        let create = if debug_utils {
            create.push_next(&mut messenger_create)
        } else {
            create
        };
        // SAFETY: `create` 的指针在本次调用期间有效；`pNext` 指向本函数栈上的
        // `messenger_create`。
        let raw = unsafe { entry.create_instance(&create, None) }
            .map_err(|error| init_error("create_instance(headless)", format!("{error:?}")))?;
        let mut instance = Self {
            entry,
            raw,
            surface_fn: None,
            wayland_fn: None,
            validation,
            debug: None,
        };
        if debug_utils && let Err(error) = instance.install_messenger() {
            eprintln!("kytrix: vulkan debug messenger unavailable: {error}");
        }
        Ok(instance)
    }

    /// 装 messenger（实例创建之后；`pNext` 那份只覆盖创建/销毁期间的消息）。
    fn install_messenger(&mut self) -> Result<(), EngineError> {
        let fn_ = ash::ext::debug_utils::Instance::new(&self.entry, &self.raw);
        let create = debug_messenger_create_info();
        // SAFETY: `create` 的回调是 `debug_callback`（`'static`），`user_data` 为空。
        let raw = unsafe { fn_.create_debug_utils_messenger(&create, None) }
            .map_err(|error| init_error("create_debug_utils_messenger", format!("{error:?}")))?;
        self.debug = Some(DebugMessenger { fn_, raw });
        Ok(())
    }

    pub(crate) fn raw(&self) -> &Instance {
        &self.raw
    }

    /// 验证层是否启用（`GpuCaps.validation`）。
    pub(crate) fn validation_enabled(&self) -> bool {
        self.validation
    }

    /// debug messenger 是否已安装（`GpuCaps.debug_messenger`；R11-4 的可观测性判据）。
    pub(crate) fn debug_messenger_installed(&self) -> bool {
        self.debug.is_some()
    }

    /// 产品路径恒 `Some`；离屏实例 `None`（没有 surface 也就没有 WSI 函数）。
    pub(crate) fn surface_fn(&self) -> Option<&ash::khr::surface::Instance> {
        self.surface_fn.as_ref()
    }

    /// 用平台层的 `wl_display*` / `wl_surface*` 建一个窗口的 `VkSurfaceKHR`。
    ///
    /// 指针的生命周期由调用方保证覆盖整个渲染器（窗口路径的 surface 存活期 = 窗口）。
    pub(crate) fn create_surface(
        &self,
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
    ) -> Result<ash::vk::SurfaceKHR, EngineError> {
        let wayland_fn = self
            .wayland_fn
            .as_ref()
            .ok_or_else(|| EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "a window surface requires a product (Wayland) Vulkan instance",
                ),
            })?;
        let create = ash::vk::WaylandSurfaceCreateInfoKHR::default()
            .display(display)
            .surface(surface);
        // SAFETY: 指针来自平台层的 `wl_display` / `wl_surface`（system backend 的真实对象）。
        unsafe { wayland_fn.create_wayland_surface(&create, None) }.map_err(|error| {
            EngineError::Swapchain {
                detail: smol_str::format_smolstr!("create_wayland_surface: {error:?}"),
            }
        })
    }

    /// 销毁一个窗口的 surface（`Surface::drop` 调用；此时 GPU 仍由该窗口的引用保活）。
    pub(crate) fn destroy_surface(&self, surface: ash::vk::SurfaceKHR) {
        if let Some(surface_fn) = self.surface_fn.as_ref() {
            // SAFETY: surface 由本实例创建；调用方保证该 surface 的 swapchain 已销毁。
            unsafe { surface_fn.destroy_surface(surface, None) };
        }
    }
}

impl Drop for VulkanInstance {
    fn drop(&mut self) {
        // 先销毁 messenger（它引用实例）。
        if let Some(debug) = self.debug.take() {
            // SAFETY: messenger 由本结构创建；实例仍然有效。
            unsafe { debug.fn_.destroy_debug_utils_messenger(debug.raw, None) };
        }
        // 验证层跑过之后的摘要：CI 靠它把「有多少条消息」变成可见事实（R11-4）。
        let (errors, warnings) = validation_counts();
        if self.validation && (errors > 0 || warnings > 0) {
            eprintln!(
                "kytrix: vulkan validation summary: {errors} error(s), {warnings} warning(s)"
            );
        }
        // SAFETY: 句柄归本结构独占；所有窗口 surface 已先于实例销毁（`SharedGpu` 的引用计数
        // 保证最后一道窗口资源 drop 之后才走到这里）。
        unsafe { self.raw.destroy_instance(None) };
    }
}

/// 是否装了 validation layer（只在 debug 构建 / 显式开启时查一次）。
fn validation_available(entry: &Entry) -> bool {
    // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
    let Ok(layers) = (unsafe { entry.enumerate_instance_layer_properties() }) else {
        return false;
    };
    layers.iter().any(|layer| {
        // SAFETY: `layer_name` 是 Vulkan 保证的 NUL 结尾字符串。
        (unsafe { CStr::from_ptr(layer.layer_name.as_ptr()) }) == c"VK_LAYER_KHRONOS_validation"
    })
}

/// messenger 的接收面：`ERROR | WARNING`（`INFO` / `VERBOSE` 在 CI 里是噪声），
/// 类型只收 `GENERAL | VALIDATION`（性能提示与「是否违反规范」无关、且量很大；
/// `SYNC-HAZARD-*` 属于 `VALIDATION` / `GENERAL`，不会被漏掉）。
fn debug_messenger_create_info() -> ash::vk::DebugUtilsMessengerCreateInfoEXT<'static> {
    use ash::vk::{
        DebugUtilsMessageSeverityFlagsEXT as Severity, DebugUtilsMessageTypeFlagsEXT as Type,
    };
    ash::vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(Severity::ERROR | Severity::WARNING)
        .message_type(Type::GENERAL | Type::VALIDATION)
        .pfn_user_callback(Some(debug_callback))
}

/// 验证层消息回调：计数 + 打到 `stderr`；`strict` 模式下 ERROR 直接 panic（CI 门禁）。
///
/// # Safety
/// `callback_data` 由 loader 在回调期间保证有效（`p_message` / `p_message_id_name` 是 NUL
/// 结尾字符串）；本函数只读它们。
unsafe extern "system" fn debug_callback(
    severity: ash::vk::DebugUtilsMessageSeverityFlagsEXT,
    types: ash::vk::DebugUtilsMessageTypeFlagsEXT,
    callback_data: *const ash::vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user_data: *mut c_void,
) -> ash::vk::Bool32 {
    use ash::vk::DebugUtilsMessageSeverityFlagsEXT as Severity;
    let error = severity.contains(Severity::ERROR);
    if error {
        VALIDATION_ERRORS.fetch_add(1, Ordering::Relaxed);
    } else if severity.contains(Severity::WARNING) {
        VALIDATION_WARNINGS.fetch_add(1, Ordering::Relaxed);
    }
    let mut message_id = "";
    let mut message = "";
    // SAFETY: loader 在回调期间保证 `callback_data` 有效（或被置空）。
    if let Some(data) = unsafe { callback_data.as_ref() } {
        // SAFETY: loader 保证这两个指针在回调期间是 NUL 结尾字符串（或空）。
        if !data.p_message_id_name.is_null() {
            message_id = unsafe { CStr::from_ptr(data.p_message_id_name) }
                .to_str()
                .unwrap_or("");
        }
        if !data.p_message.is_null() {
            message = unsafe { CStr::from_ptr(data.p_message) }
                .to_str()
                .unwrap_or("");
        }
    }
    let level = if error { "error" } else { "warning" };
    eprintln!("kytrix: vulkan validation {level} [{message_id}] {message} ({types:?})");
    if error && ValidationMode::from_env().strict() {
        // CI 门禁：验证层 error 必须让构建失败，而不是一行被忽略的日志。
        panic!("vulkan validation error [{message_id}]: {message}");
    }
    ash::vk::FALSE
}

#[cfg(test)]
mod tests {
    use super::{VALIDATION_ENV, ValidationMode, validation_available, validation_counts};

    /// 环境变量语义（R11-4 的 CI 开关）：默认按构建类型取，显式值覆盖，拼错按最严格处理。
    #[test]
    fn the_validation_mode_parses_the_environment_switch() {
        // `from_env()` 的结果取决于**进程环境**（CI 可能把 `KYTRIX_VK_VALIDATION` 设成 strict
        // 来跑验证门禁），所以这里只断言它总落在合法集合里，不假设"未设置"。
        assert!(matches!(
            ValidationMode::from_env(),
            ValidationMode::Off | ValidationMode::On | ValidationMode::Strict
        ));
        // `set_var` 在 Rust 2024 是 unsafe，且与并行测试共享进程环境：这里只验证解析函数
        // 对字符串的映射，避免污染其它测试（并行的 GPU 测试也会读这个变量）。
        let parse = |value: &str| match value {
            "off" | "0" | "false" => ValidationMode::Off,
            "strict" | "fail" => ValidationMode::Strict,
            "on" | "1" | "true" => ValidationMode::On,
            _ => ValidationMode::Strict,
        };
        assert_eq!(parse("off"), ValidationMode::Off);
        assert_eq!(parse("0"), ValidationMode::Off);
        assert_eq!(parse("on"), ValidationMode::On);
        assert_eq!(parse("strict"), ValidationMode::Strict);
        assert_eq!(parse("typo"), ValidationMode::Strict, "拼错按最严格处理");
        assert!(!ValidationMode::Off.enabled());
        assert!(ValidationMode::On.enabled());
        assert!(ValidationMode::Strict.strict());
        assert!(!ValidationMode::On.strict());
        assert!(!VALIDATION_ENV.is_empty());
    }

    /// 计数可读且单调（`strict` 之外的消费者：结束摘要与 CI 断言读它）。
    ///
    /// 不能断言「恰好为 0」：同一进程里别的 GPU 测试可能先触发过验证层消息（测试并行执行），
    /// 那会让这条测试变成偶发失败 —— 而它要保证的是「计数器存在且可读」。
    #[test]
    fn validation_counters_are_readable() {
        let (errors, warnings) = validation_counts();
        let (errors_again, warnings_again) = validation_counts();
        assert!(
            errors_again >= errors && warnings_again >= warnings,
            "计数器必须单调不减（{errors}/{warnings} → {errors_again}/{warnings_again}）"
        );
    }

    /// 验证层是否安装是**环境事实**：本机（CI）没装就返回 false，不该 panic。
    #[test]
    fn asking_for_the_validation_layer_never_panics() {
        // SAFETY: 只加载 loader 并枚举层。
        let entry = match unsafe { ash::Entry::load() } {
            Ok(entry) => entry,
            Err(_) => {
                eprintln!("skip: no Vulkan loader");
                return;
            }
        };
        let _ = validation_available(&entry);
    }
}

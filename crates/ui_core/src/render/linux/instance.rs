//! `VkInstance` + `VK_KHR_wayland_surface`（`design/phase-12-macos-linux.md` §6.1）。
//!
//! `display` / `surface` 是 **`wl_display*` / `wl_surface*`**（由 `platform/linux` 提供；
//! 平台层因此必须启用 `wayland-client` 的 `system` feature —— 纯 Rust 后端的对象指针是令牌，
//! 不能交给 Vulkan 驱动，F-135）。

use std::ffi::CStr;

use ash::{Entry, Instance};

use super::device::init_error;
use crate::error::EngineError;

/// 实例级必需的扩展。
const REQUIRED_EXTENSIONS: [&CStr; 2] = [ash::khr::surface::NAME, ash::khr::wayland_surface::NAME];

/// 实例 + Wayland surface。
pub(crate) struct VulkanInstance {
    /// 只为保活（实例创建之后 loader 入口仍须有效）。
    _entry: Entry,
    raw: Instance,
    /// 离屏（快照）实例没有 surface：`null` 句柄，`surface_fn` / `_wayland_fn` 也不存在。
    surface: ash::vk::SurfaceKHR,
    surface_fn: Option<ash::khr::surface::Instance>,
    /// 只为保活（surface 已创建；销毁走 `surface_fn`）。
    _wayland_fn: Option<ash::khr::wayland_surface::Instance>,
}

impl VulkanInstance {
    /// 建实例与 surface（`display` / `surface` 的生命周期由调用方保证覆盖整个渲染器）。
    pub(crate) fn new(
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
    ) -> Result<Self, EngineError> {
        // SAFETY: `Entry::load` 只加载 `libvulkan.so.1`（不碰传入指针）。
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
        let extension_pointers: Vec<*const std::ffi::c_char> = REQUIRED_EXTENSIONS
            .iter()
            .map(|name| name.as_ptr())
            .collect();
        let mut layers = Vec::new();
        if cfg!(debug_assertions) && validation_available(&entry) {
            layers.push(c"VK_LAYER_KHRONOS_validation".as_ptr());
        }
        let create = ash::vk::InstanceCreateInfo::default()
            .application_info(&application)
            .enabled_extension_names(&extension_pointers)
            .enabled_layer_names(&layers);
        // SAFETY: `create` 的指针都在本次调用期间有效；没有 `pNext` 链。
        let raw = unsafe { entry.create_instance(&create, None) }
            .map_err(|error| init_error("create_instance", format!("{error:?}")))?;

        let wayland_fn = ash::khr::wayland_surface::Instance::new(&entry, &raw);
        let surface_fn = ash::khr::surface::Instance::new(&entry, &raw);
        let surface_create = ash::vk::WaylandSurfaceCreateInfoKHR::default()
            .display(display)
            .surface(surface);
        // SAFETY: 指针来自平台层的 `wl_display` / `wl_surface`（system backend 的真实对象）。
        let surface = unsafe { wayland_fn.create_wayland_surface(&surface_create, None) }.map_err(
            |error| EngineError::Swapchain {
                detail: smol_str::format_smolstr!("create_wayland_surface: {error:?}"),
            },
        )?;
        Ok(Self {
            _entry: entry,
            raw,
            surface,
            surface_fn: Some(surface_fn),
            _wayland_fn: Some(wayland_fn),
        })
    }

    /// 离屏实例（无窗口）：不建 surface，也不启用 WSI 扩展。
    ///
    /// Linux 的产品**窗口**路径需要真实 `wl_surface`（F-137），但「没有窗口也要画」的路径
    /// （`Window::enable_offscreen_renderer`、像素快照）只需要一个能渲染到 `VkImage` 的设备。
    pub(crate) fn new_headless() -> Result<Self, EngineError> {
        // SAFETY: `Entry::load` 只加载 `libvulkan.so.1`。
        let entry =
            unsafe { Entry::load() }.map_err(|error| init_error("libvulkan", error.to_string()))?;
        let application = ash::vk::ApplicationInfo::default()
            .application_name(c"kytrix")
            .application_version(1)
            .engine_name(c"ui_core")
            .api_version(ash::vk::make_api_version(0, 1, 2, 0));
        let mut layers = Vec::new();
        if cfg!(debug_assertions) && validation_available(&entry) {
            layers.push(c"VK_LAYER_KHRONOS_validation".as_ptr());
        }
        let create = ash::vk::InstanceCreateInfo::default()
            .application_info(&application)
            .enabled_layer_names(&layers);
        // SAFETY: `create` 的指针都在本次调用期间有效；没有 `pNext` 链。
        let raw = unsafe { entry.create_instance(&create, None) }
            .map_err(|error| init_error("create_instance(headless)", format!("{error:?}")))?;
        Ok(Self {
            _entry: entry,
            raw,
            surface: ash::vk::SurfaceKHR::null(),
            surface_fn: None,
            _wayland_fn: None,
        })
    }

    pub(crate) fn raw(&self) -> &Instance {
        &self.raw
    }

    pub(crate) fn surface(&self) -> ash::vk::SurfaceKHR {
        self.surface
    }

    /// 产品路径恒 `Some`；离屏实例 `None`（没有 surface 也就没有 WSI 函数）。
    pub(crate) fn surface_fn(&self) -> Option<&ash::khr::surface::Instance> {
        self.surface_fn.as_ref()
    }
}

impl Drop for VulkanInstance {
    fn drop(&mut self) {
        // SAFETY: 句柄归本结构独占；`destroy_surface` 之前不会有队列提交。
        unsafe {
            if let Some(surface_fn) = self.surface_fn.as_ref() {
                surface_fn.destroy_surface(self.surface, None);
            }
            self.raw.destroy_instance(None);
        }
    }
}

/// 是否装了 validation layer（只在 debug 构建里查一次）。
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

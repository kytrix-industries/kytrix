//! 进程级 `VkInstance` + `VK_KHR_wayland_surface`（`design/phase-12-macos-linux.md` §6.1）。
//!
//! 实例是**进程级共享**的（P1-2.9）：多窗口只建一个 `VkInstance`，每个窗口用
//! [`VulkanInstance::create_surface`] 建自己的 `VkSurfaceKHR`（surface 的存活期 = 窗口，
//! 由 `swapchain::Surface` 在窗口 drop 时销毁）。实例本身由 `device::SharedGpu` 持有，
//! 最后一个窗口关闭、`SharedGpu` 引用计数归零时才销毁 —— 因此 surface 必然先于 instance 销毁。
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

/// 进程级实例 + 每窗口 surface 的创建入口。
pub(crate) struct VulkanInstance {
    /// 只为保活（实例创建之后 loader 入口仍须有效）。
    _entry: Entry,
    raw: Instance,
    /// 产品路径才有 WSI 函数表；离屏实例（快照 / 单测）没有 surface。
    surface_fn: Option<ash::khr::surface::Instance>,
    /// 只为保活（surface 由本实例创建，销毁走 `surface_fn`）。
    wayland_fn: Option<ash::khr::wayland_surface::Instance>,
}

impl VulkanInstance {
    /// 产品路径的进程级实例：启用 `VK_KHR_surface` + `VK_KHR_wayland_surface`。
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
        Ok(Self {
            surface_fn: Some(ash::khr::surface::Instance::new(&entry, &raw)),
            wayland_fn: Some(ash::khr::wayland_surface::Instance::new(&entry, &raw)),
            _entry: entry,
            raw,
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
            surface_fn: None,
            wayland_fn: None,
        })
    }

    pub(crate) fn raw(&self) -> &Instance {
        &self.raw
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
        // SAFETY: 句柄归本结构独占；所有窗口 surface 已先于实例销毁（`SharedGpu` 的引用计数
        // 保证最后一道窗口资源 drop 之后才走到这里）。
        unsafe { self.raw.destroy_instance(None) };
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

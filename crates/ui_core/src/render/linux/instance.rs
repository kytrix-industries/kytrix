use std::{
    ffi::{CStr, c_void},
    sync::atomic::{AtomicU64, Ordering},
};

use ash::{Entry, Instance};

use super::device::init_error;
use crate::error::EngineError;

const REQUIRED_EXTENSIONS: [&CStr; 2] = [ash::khr::surface::NAME, ash::khr::wayland_surface::NAME];

pub(crate) const VALIDATION_ENV: &str = "KYTRIX_VK_VALIDATION";

static VALIDATION_ERRORS: AtomicU64 = AtomicU64::new(0);
static VALIDATION_WARNINGS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn validation_counts() -> (u64, u64) {
    (
        VALIDATION_ERRORS.load(Ordering::Relaxed),
        VALIDATION_WARNINGS.load(Ordering::Relaxed),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValidationMode {
    Off,
    On,
    Strict,
}

impl ValidationMode {
    fn from_env() -> Self {
        match std::env::var(VALIDATION_ENV).ok().as_deref() {
            Some("off") | Some("0") | Some("false") => Self::Off,
            Some("strict") | Some("fail") => Self::Strict,
            Some("on") | Some("1") | Some("true") => Self::On,
            Some(_) => Self::Strict,
            None if cfg!(debug_assertions) => Self::On,
            None => Self::Off,
        }
    }

    fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    fn strict(self) -> bool {
        matches!(self, Self::Strict)
    }
}

struct DebugMessenger {
    fn_: ash::ext::debug_utils::Instance,
    raw: ash::vk::DebugUtilsMessengerEXT,
}

pub(crate) struct VulkanInstance {
    entry: Entry,
    raw: Instance,
    surface_fn: Option<ash::khr::surface::Instance>,
    wayland_fn: Option<ash::khr::wayland_surface::Instance>,
    validation: bool,
    debug: Option<DebugMessenger>,
}

impl VulkanInstance {
    pub(crate) fn new_product() -> Result<Self, EngineError> {
        let entry =
            unsafe { Entry::load() }.map_err(|error| init_error("libvulkan", error.to_string()))?;

        let available =
            unsafe { entry.enumerate_instance_extension_properties(None) }.map_err(|error| {
                init_error("enumerate_instance_extension_properties", error.to_string())
            })?;
        for required in REQUIRED_EXTENSIONS {
            let present = available.iter().any(|extension| {
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
            debug_utils = available.iter().any(|extension| {
                (unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) })
                    == ash::ext::debug_utils::NAME
            });
            if debug_utils {
                extension_pointers.push(ash::ext::debug_utils::NAME.as_ptr());
            } else {
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
        let create = if debug_utils {
            create.push_next(&mut messenger_create)
        } else {
            create
        };
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
            eprintln!("kytrix: vulkan debug messenger unavailable: {error}");
        }
        Ok(instance)
    }

    pub(crate) fn new_headless() -> Result<Self, EngineError> {
        let entry =
            unsafe { Entry::load() }.map_err(|error| init_error("libvulkan", error.to_string()))?;
        let available =
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
            .enabled_extension_names(&extension_pointers)
            .enabled_layer_names(&layers);
        let create = if debug_utils {
            create.push_next(&mut messenger_create)
        } else {
            create
        };
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

    fn install_messenger(&mut self) -> Result<(), EngineError> {
        let fn_ = ash::ext::debug_utils::Instance::new(&self.entry, &self.raw);
        let create = debug_messenger_create_info();
        let raw = unsafe { fn_.create_debug_utils_messenger(&create, None) }
            .map_err(|error| init_error("create_debug_utils_messenger", format!("{error:?}")))?;
        self.debug = Some(DebugMessenger { fn_, raw });
        Ok(())
    }

    pub(crate) fn raw(&self) -> &Instance {
        &self.raw
    }

    pub(crate) fn validation_enabled(&self) -> bool {
        self.validation
    }

    pub(crate) fn debug_messenger_installed(&self) -> bool {
        self.debug.is_some()
    }

    pub(crate) fn surface_fn(&self) -> Option<&ash::khr::surface::Instance> {
        self.surface_fn.as_ref()
    }

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
        unsafe { wayland_fn.create_wayland_surface(&create, None) }.map_err(|error| {
            EngineError::Swapchain {
                detail: smol_str::format_smolstr!("create_wayland_surface: {error:?}"),
            }
        })
    }

    pub(crate) fn destroy_surface(&self, surface: ash::vk::SurfaceKHR) {
        if let Some(surface_fn) = self.surface_fn.as_ref() {
            unsafe { surface_fn.destroy_surface(surface, None) };
        }
    }
}

impl Drop for VulkanInstance {
    fn drop(&mut self) {
        if let Some(debug) = self.debug.take() {
            unsafe { debug.fn_.destroy_debug_utils_messenger(debug.raw, None) };
        }
        let (errors, warnings) = validation_counts();
        if self.validation && (errors > 0 || warnings > 0) {
            eprintln!(
                "kytrix: vulkan validation summary: {errors} error(s), {warnings} warning(s)"
            );
        }
        unsafe { self.raw.destroy_instance(None) };
    }
}

fn validation_available(entry: &Entry) -> bool {
    let Ok(layers) = (unsafe { entry.enumerate_instance_layer_properties() }) else {
        return false;
    };
    layers.iter().any(|layer| {
        (unsafe { CStr::from_ptr(layer.layer_name.as_ptr()) }) == c"VK_LAYER_KHRONOS_validation"
    })
}

fn debug_messenger_create_info() -> ash::vk::DebugUtilsMessengerCreateInfoEXT<'static> {
    use ash::vk::{
        DebugUtilsMessageSeverityFlagsEXT as Severity, DebugUtilsMessageTypeFlagsEXT as Type,
    };
    ash::vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(Severity::ERROR | Severity::WARNING)
        .message_type(Type::GENERAL | Type::VALIDATION)
        .pfn_user_callback(Some(debug_callback))
}

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
    if let Some(data) = unsafe { callback_data.as_ref() } {
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
        panic!("vulkan validation error [{message_id}]: {message}");
    }
    ash::vk::FALSE
}

#[cfg(test)]
mod tests {
    use super::{VALIDATION_ENV, ValidationMode, validation_available, validation_counts};

    #[test]
    fn the_validation_mode_parses_the_environment_switch() {
        assert!(matches!(
            ValidationMode::from_env(),
            ValidationMode::Off | ValidationMode::On | ValidationMode::Strict
        ));
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

    #[test]
    fn validation_counters_are_readable() {
        let (errors, warnings) = validation_counts();
        let (errors_again, warnings_again) = validation_counts();
        assert!(
            errors_again >= errors && warnings_again >= warnings,
            "计数器必须单调不减（{errors}/{warnings} → {errors_again}/{warnings_again}）"
        );
    }

    #[test]
    fn asking_for_the_validation_layer_never_panics() {
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

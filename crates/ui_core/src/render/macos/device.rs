use std::path::PathBuf;

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_foundation::NSError;
use objc2_metal::{
    MTLArgumentBuffersTier, MTLCommandBufferError, MTLDevice, MTLResource, MTLTexture,
};
use smol_str::SmolStr;

use crate::{error::EngineError, render::retire::GpuResourceHandle};

thread_local! {
    static ON_GPU_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn enter_gpu_thread() {
    ON_GPU_THREAD.set(true);
}

pub(crate) fn on_gpu_thread() -> bool {
    ON_GPU_THREAD.get()
}

pub(crate) const D8_REQUIRED: &str =
    "Metal device / Argument Buffers Tier 2 / maxBufferLength >= 256 MiB";

const MIN_MAX_BUFFER_LENGTH: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuCaps {
    pub adapter: SmolStr,
    pub registry_id: u64,
    pub unified_memory: bool,
    pub low_power: bool,
    pub max_buffer_length: u64,
    pub argument_buffers_tier: i32,
}

impl GpuCaps {
    pub(crate) fn collect(device: &ProtocolObject<dyn MTLDevice>) -> Self {
        Self {
            adapter: device_name(device),
            registry_id: device.registryID(),
            unified_memory: device.hasUnifiedMemory(),
            low_power: device.isLowPower(),
            max_buffer_length: device.maxBufferLength() as u64,
            argument_buffers_tier: device.argumentBuffersSupport().0 as i32,
        }
    }

    pub(crate) fn baseline_ok(&self) -> bool {
        self.argument_buffers_tier >= MTLArgumentBuffersTier::Tier2.0 as i32
            && self.max_buffer_length >= MIN_MAX_BUFFER_LENGTH
    }

    pub(crate) fn actual(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "ArgumentBuffersTier {} / maxBufferLength {} MiB",
            if self.argument_buffers_tier >= MTLArgumentBuffersTier::Tier2.0 as i32 {
                "Tier2"
            } else {
                "Tier1"
            },
            self.max_buffer_length / (1024 * 1024)
        )
    }

    pub(crate) fn describe(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "adapter={:?} registry_id=0x{:X} unified={} low_power={} max_buffer={}MiB argument_buffers={}",
            self.adapter,
            self.registry_id,
            self.unified_memory,
            self.low_power,
            self.max_buffer_length / (1024 * 1024),
            if self.argument_buffers_tier >= 1 {
                "Tier2"
            } else {
                "Tier1"
            }
        )
    }

    pub(crate) fn check(&self) -> Result<(), EngineError> {
        if self.baseline_ok() {
            Ok(())
        } else {
            Err(EngineError::UnsupportedGpu {
                os: "macos",
                required: D8_REQUIRED,
                actual: self.actual(),
            })
        }
    }
}

const HANDLE_KIND_OBJECT: u8 = 1;

pub(crate) enum GpuObject {
    Texture(Retained<ProtocolObject<dyn MTLTexture>>),
    Buffer(Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>),
}

impl GpuObject {
    fn label(&self) -> smol_str::SmolStr {
        match self {
            Self::Texture(texture) => texture
                .label()
                .map(|label| smol_str::SmolStr::new(label.to_string()))
                .unwrap_or_else(|| smol_str::SmolStr::new_static("texture")),
            Self::Buffer(buffer) => buffer
                .label()
                .map(|label| smol_str::SmolStr::new(label.to_string()))
                .unwrap_or_else(|| smol_str::SmolStr::new_static("buffer")),
        }
    }
}

pub(crate) struct Device {
    raw: Retained<ProtocolObject<dyn MTLDevice>>,
    resources: ResourceTable,
    lost: Option<LostInfo>,
}

struct LostInfo {
    reason: SmolStr,
    log_path: PathBuf,
}

impl Device {
    pub(crate) fn new() -> Result<Self, EngineError> {
        let core = crate::render::macos::core::GpuCore::shared()?;
        Ok(Self {
            raw: core.retain_device(),
            resources: ResourceTable::new(),
            lost: None,
        })
    }

    pub(crate) fn raw(&self) -> &ProtocolObject<dyn MTLDevice> {
        &self.raw
    }

    pub(crate) fn allocate(&mut self, object: GpuObject) -> GpuResourceHandle {
        self.resources.insert(object)
    }

    pub(crate) fn release(&mut self, handle: GpuResourceHandle) -> bool {
        self.resources.release(handle)
    }

    pub(crate) fn lost_error(&self) -> Option<EngineError> {
        self.lost.as_ref().map(|lost| EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        })
    }

    pub(crate) fn note_failure(
        &mut self,
        error: Option<&NSError>,
        frame: u64,
    ) -> Option<EngineError> {
        if self.lost.is_some() {
            return self.lost_error();
        }
        let error = error?;
        if error.domain().to_string() != "MTLCommandBufferError" {
            return None;
        }
        let code = error.code();
        let access_revoked = MTLCommandBufferError::AccessRevoked.0 as isize;
        let device_removed = MTLCommandBufferError::DeviceRemoved.0 as isize;
        if code != access_revoked && code != device_removed {
            return None;
        }
        let reason = smol_str::format_smolstr!(
            "MTLCommandBufferError({code}) {}",
            error.localizedDescription().to_string()
        );
        let log_path = write_diagnostics(&reason, frame);
        let lost = LostInfo { reason, log_path };
        let error = EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        };
        self.lost = Some(lost);
        Some(error)
    }
}

pub(crate) fn init_error(context: &'static str, detail: impl std::fmt::Display) -> EngineError {
    EngineError::AppInit {
        detail: smol_str::format_smolstr!("metal {context}: {detail}"),
    }
}

fn device_name(device: &ProtocolObject<dyn MTLDevice>) -> SmolStr {
    SmolStr::new(device.name().to_string())
}

pub(crate) fn write_diagnostics(reason: &str, frame: u64) -> PathBuf {
    let path = std::env::temp_dir().join(smol_str::format_smolstr!(
        "kytrix-metal-{}-{frame}.txt",
        std::process::id()
    ));
    let body = smol_str::format_smolstr!("kytrix metal command buffer failure\nreason: {reason}\n");
    let _ = std::fs::write(&path, body.as_bytes());
    path
}

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
        let dropped = self.slots[index].take();
        debug_assert!(
            dropped.is_some_and(|object| !object.label().is_empty()),
            "released slot must hold a labelled resource"
        );
        self.free.push(index as u32);
        true
    }
}

#[cfg(test)]
mod tests {
    use objc2::rc::Retained;
    use objc2_foundation::{NSError, NSString};
    use objc2_metal::MTLCommandBufferError;

    use super::Device;
    use crate::error::EngineError;

    fn classify(domain: &str, code: isize) -> Option<EngineError> {
        let mut device = Device::new().expect("metal device");
        let domain = NSString::from_str(domain);
        let error: Retained<NSError> =
            unsafe { NSError::errorWithDomain_code_userInfo(&domain, code, None) };
        device.note_failure(Some(&error), 7)
    }

    #[test]
    fn only_access_revoked_and_device_removed_classify_as_device_lost() {
        for code in [
            MTLCommandBufferError::AccessRevoked.0 as isize,
            MTLCommandBufferError::DeviceRemoved.0 as isize,
        ] {
            match classify("MTLCommandBufferError", code) {
                Some(EngineError::DeviceLost { reason, log_path }) => {
                    assert!(
                        reason.contains("MTLCommandBufferError"),
                        "分类理由必须可诊断：{reason}"
                    );
                    assert!(
                        log_path.exists(),
                        "DeviceLost 必须落盘诊断文件：{log_path:?}"
                    );
                }
                other => panic!("code {code} 必须升级为 DeviceLost：{other:?}"),
            }
        }
        for code in [
            MTLCommandBufferError::StackOverflow.0 as isize,
            MTLCommandBufferError::Internal.0 as isize,
            99,
        ] {
            assert!(
                classify("MTLCommandBufferError", code).is_none(),
                "code {code} 不是设备丢失：必须保持非 DeviceLost（由调用方归 Swapchain）"
            );
        }
        assert!(
            classify(
                "NSErrorDomainUnrelated",
                MTLCommandBufferError::DeviceRemoved.0 as isize
            )
            .is_none(),
            "其它错误域的同码值含义不同，不能当设备丢失"
        );
        let mut device = Device::new().expect("metal device");
        assert!(device.note_failure(None, 1).is_none());
    }

    #[test]
    fn device_loss_is_sticky_for_the_whole_device() {
        use std::path::PathBuf;

        let mut device = Device::new().expect("metal device");
        let domain = NSString::from_str("MTLCommandBufferError");
        let removed = unsafe {
            NSError::errorWithDomain_code_userInfo(
                &domain,
                MTLCommandBufferError::DeviceRemoved.0 as isize,
                None,
            )
        };
        assert!(matches!(
            device.note_failure(Some(&removed), 1),
            Some(EngineError::DeviceLost { .. })
        ));
        match device.note_failure(None, 2) {
            Some(EngineError::DeviceLost { reason, log_path }) => {
                assert!(reason.contains("MTLCommandBufferError"), "{reason}");
                assert_ne!(log_path, PathBuf::new());
            }
            other => panic!("粘性 DeviceLost 必须被再次报告：{other:?}"),
        }
        assert!(device.lost_error().is_some());
    }
}

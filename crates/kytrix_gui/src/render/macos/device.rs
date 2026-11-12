//! `MTLDevice` 的创建、D8 采集与资源句柄表（`design/phase-12-macos-linux.md` §5.1）。
//!
//! D8 硬基线：**`MTLCreateSystemDefaultDevice` 非空 + `argumentBuffersSupport == Tier2`**。
//! v1 不建 argument buffer（F-119），但 Tier2 仍是产品基线（主设计 §2.2）与未来多页图集的门。

use std::path::PathBuf;

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_foundation::NSError;
use objc2_metal::{
    MTLArgumentBuffersTier, MTLCommandBufferError, MTLDevice, MTLResource, MTLTexture,
};
use smol_str::SmolStr;

use crate::{error::EngineError, render::retire::GpuResourceHandle};

/// D8 硬基线（错误文案与安装器共用的唯一来源）。
pub(crate) const D8_REQUIRED: &str =
    "Metal device / Argument Buffers Tier 2 / maxBufferLength >= 256 MiB";

/// `maxBufferLength` 的下限（256 MiB）：三张 2048² 上传缓冲 + 实例流远小于此值。
const MIN_MAX_BUFFER_LENGTH: u64 = 256 * 1024 * 1024;

/// 启动时一次性采集的能力快照（诊断字段不参与基线）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuCaps {
    /// 设备名（`MTLDevice.name` 不可得时用 `"Apple GPU"`）。
    pub adapter: SmolStr,
    /// `MTLDevice.registryID`。
    pub registry_id: u64,
    /// `hasUnifiedMemory`（Apple Silicon / Intel 集显为 `true`）。
    pub unified_memory: bool,
    /// `isLowPower`（集显标记）。
    pub low_power: bool,
    /// `maxBufferLength`（字节）。
    pub max_buffer_length: u64,
    /// `argumentBuffersSupport().0`（`0` = Tier1，`1` = Tier2）。
    pub argument_buffers_tier: i32,
}

impl GpuCaps {
    /// 硬基线：**Argument Buffers Tier 2** + `maxBufferLength >= 256 MiB`
    /// （实例/裁剪/上传缓冲都是大 buffer；`maxBufferLength` 是 Apple GPU 的硬上限）。
    pub(crate) fn baseline_ok(&self) -> bool {
        self.argument_buffers_tier >= MTLArgumentBuffersTier::Tier2.0 as i32
            && self.max_buffer_length >= MIN_MAX_BUFFER_LENGTH
    }

    /// `UnsupportedGpu.actual` 的稳定格式（只含基线项）。
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

    /// 全字段诊断行（不含时间戳）。
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

    /// 唯一校验入口：失败 → `EngineError::UnsupportedGpu`（I10，不降级）。
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

/// `GpuResourceHandle` 的 `kind`：Metal 资源（texture / buffer）。
const HANDLE_KIND_OBJECT: u8 = 1;

/// 句柄表里的资源（两种具体类型；objc2 0.6 没有 `MTLTexture → MTLResource` 的协议上转，因此用枚举）。
pub(crate) enum GpuObject {
    /// `MTLTexture`（离屏目标 / 图集页）。
    Texture(Retained<ProtocolObject<dyn MTLTexture>>),
    /// `MTLBuffer`（staging / 实例 / 裁剪）。
    Buffer(Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>),
}

impl GpuObject {
    /// 资源的 `label`（Metal 的调试名；未命名时给稳定占位）。
    ///
    /// 这里读 `Retained` 里的对象，既让诊断可读，也避免句柄表的枚举字段成为「只写不读」。
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

/// 一个设备 + 资源句柄表 + 丢失状态。
pub(crate) struct Device {
    raw: Retained<ProtocolObject<dyn MTLDevice>>,
    resources: ResourceTable,
    lost: Option<LostInfo>,
}

/// 设备丢失的粘性记录（`EngineError` 不能 `Clone`，存字段按需重建）。
struct LostInfo {
    reason: SmolStr,
    log_path: PathBuf,
}

impl Device {
    /// 产品路径：系统默认设备 → 采 caps → 校验 D8。
    pub(crate) fn new() -> Result<Self, EngineError> {
        let raw =
            objc2_metal::MTLCreateSystemDefaultDevice().ok_or(EngineError::UnsupportedGpu {
                os: "macos",
                required: D8_REQUIRED,
                actual: SmolStr::new_static("no Metal device"),
            })?;
        let caps = GpuCaps {
            adapter: device_name(&raw),
            registry_id: raw.registryID(),
            unified_memory: raw.hasUnifiedMemory(),
            low_power: raw.isLowPower(),
            max_buffer_length: raw.maxBufferLength() as u64,
            argument_buffers_tier: raw.argumentBuffersSupport().0 as i32,
        };
        caps.check()?;
        // 诊断行只在启动失败时有消费者；这里断言它可格式化（`AppInit` 的 detail 会带上）。
        debug_assert!(!caps.describe().is_empty());
        Ok(Self {
            raw,
            resources: ResourceTable::new(),
            lost: None,
        })
    }

    pub(crate) fn raw(&self) -> &ProtocolObject<dyn MTLDevice> {
        &self.raw
    }

    /// 登记一个资源，返回可放进 `RetireQueue` 的句柄。
    pub(crate) fn allocate(&mut self, object: GpuObject) -> GpuResourceHandle {
        self.resources.insert(object)
    }

    /// 释放句柄对应的资源；陈旧句柄返回 `false`。
    pub(crate) fn release(&mut self, handle: GpuResourceHandle) -> bool {
        self.resources.release(handle)
    }

    /// 粘性丢失错误（`Some` 之后不得再提交）。
    pub(crate) fn lost_error(&self) -> Option<EngineError> {
        self.lost.as_ref().map(|lost| EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        })
    }

    /// 命令缓冲失败后的分类：`MTLCommandBufferError::DeviceRemoved` / `AccessRevoked` 升级为
    /// 粘性 `DeviceLost`（并落诊断文件）；其余错误原样交给调用方（`Swapchain` / `ShaderLoad`）。
    pub(crate) fn note_failure(
        &mut self,
        error: Option<&NSError>,
        frame: u64,
    ) -> Option<EngineError> {
        if self.lost.is_some() {
            return self.lost_error();
        }
        let error = error?;
        // 只认 `MTLCommandBufferError` 域的码（别的域同码值含义完全不同）。
        if error.domain().to_string() != "MTLCommandBufferError" {
            return None;
        }
        let code = error.code();
        // 用 Metal 的枚举常量，不用魔数：`AccessRevoked = 4`、`DeviceRemoved = 11`、
        // `StackOverflow = 12`（历史实现把 12 当 AccessRevoked 是错的）。
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

/// `AppInit` 的 `detail`（构造期失败的统一格式）。
pub(crate) fn init_error(context: &'static str, detail: impl std::fmt::Display) -> EngineError {
    EngineError::AppInit {
        detail: smol_str::format_smolstr!("metal {context}: {detail}"),
    }
}

/// `MTLDevice.name`（取不到时给稳定占位，不让诊断行出现空值）。
fn device_name(device: &ProtocolObject<dyn MTLDevice>) -> SmolStr {
    SmolStr::new(device.name().to_string())
}

/// 设备丢失诊断：写一行到 `%TEMP%`（写失败只降级为路径本身）。
fn write_diagnostics(reason: &str, frame: u64) -> PathBuf {
    let path = std::env::temp_dir().join(smol_str::format_smolstr!(
        "kytrix-metal-{}-{frame}.txt",
        std::process::id()
    ));
    let body = smol_str::format_smolstr!("kytrix metal command buffer failure\nreason: {reason}\n");
    let _ = std::fs::write(&path, body.as_bytes());
    path
}

/// 资源句柄表（generation 拒绝陈旧句柄；只有 `kind = 1`）。
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

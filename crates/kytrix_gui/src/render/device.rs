//! D8 基线与能力快照（`design/phase-08-d3d12.md` §2.2 / §4.1）。
//!
//! 纯数据 + 纯函数：不碰 D3D12、不做 I/O、不写日志。唯一消费者是
//! `render/windows/device.rs`（建完设备后校验）与 `EngineError::UnsupportedGpu` 的 `actual`。
//!
//! 采集顺序与硬基线见主设计 §2.2 的 8 项表；**`HighestShaderModel` 必须先填期望上限再查**
//! （DEVIATIONS F-73：填 `6_0` 时同一台设备会返回 `6_0`）。

use smol_str::SmolStr;

use crate::error::EngineError;

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
    /// 全字段诊断行（日志 / A-B 对照；不含时间戳）。P09 的日志层接上后解除 `cfg`。
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

#[cfg(test)]
mod tests {
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

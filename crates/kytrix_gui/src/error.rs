//! 引擎公开错误面。
//!
//! 只有强类型变体，不出现字符串错误或通用错误包装（AGENTS §3）。`Display` 为英文诊断文本，
//! 只用于日志 / CI / 开发者；面向用户的本地化文案由应用层按变体生成（i18n 留应用层）。
//!
//! 变体表在 `design/phase-01-foundation.md` §6.2.2 冻结为 17 项，本阶段实现不依赖后续阶段类型的
//! 9 项：D8 基线五类 + [`EngineError::InvalidDpiScale`] + [`EngineError::StaleNode`] /
//! [`EngineError::CyclicDerived`]（P03）+ [`EngineError::InvalidMeasure`]（P04）。其余变体随对应阶段落地
//! （`LayoutInvalid` 无生产者、继续留空，`GeometryRegister` → P09，
//! `AtlasExhausted` / `TextShape` → **P05**（DEVIATIONS F-55），`AppInit` → P08，
//! `WindowCreate` → P12，`ResourceDecode` → P15，`A11yInit` → P14）。
//! 完整「名称 + payload + 实现 Phase」表见 `design/phase-01-foundation.md` §6.2.2；
//! `StaleNode` / `CyclicDerived` 的实现 Phase 由 P04 改为 P03（`design/DEVIATIONS.md` S-30），
//! `InvalidMeasure` 由 P05 改为 P04（DEVIATIONS F-51）。

use std::path::PathBuf;

use smol_str::SmolStr;

use crate::{
    layout::Axis,
    node::{id::NodeId, signal::DerivedId},
    paint::atlas::AtlasKind,
    window::WindowId,
};

/// 引擎公开错误枚举。
///
/// 全部变体的 `source()` 均为 `None`：不包装外部错误类型，外部错误码（HRESULT / `VkResult` /
/// `MTLCommandBuffer` error）降为结构字段或可读文本，保证调用方可以穷尽 match 后决定处理策略。
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// D8 基线不满足。`actual` 按 `required` 的项顺序逐项拼接，便于人读比对。
    ///
    /// 文案必须同时给出实际值与最低要求、指向发布说明，且**不得**出现降级承诺（I10）。
    #[error(
        "GPU baseline not met: {os} requires {required}; detected {actual}. \
         This build has no software or no-GPU fallback; see the release notes for the supported \
         hardware baseline."
    )]
    UnsupportedGpu {
        os: &'static str,
        required: &'static str,
        actual: SmolStr,
    },

    /// 图形设备丢失。`log_path` 为已落盘的 DRED / validation layer 诊断文件。
    #[error("graphics device lost: {reason}; diagnostics written to {log_path}")]
    DeviceLost { reason: SmolStr, log_path: PathBuf },

    /// Wayland 合成器不满足基线（缺 `xdg_wm_base` / 版本不足 / 无 `wl_output` 等）。Linux 不回退其它窗口系统。
    #[error("Wayland compositor does not meet requirements: {detail}")]
    WaylandCompositor { detail: SmolStr },

    /// shader 产物加载失败。`target` 为 `"dxil"` / `"metallib"` / `"spirv"`。
    #[error("failed to load shader artifact for {target} at {path}")]
    ShaderLoad { target: &'static str, path: PathBuf },

    /// swapchain / offscreen 重建失败。
    #[error("swapchain reconfiguration failed: {detail}")]
    Swapchain { detail: SmolStr },

    /// 平台报告的 DPI 缩放因子非法（非有限值或 `<= 0`）。唯一生产者是平台层 DPI 事件处理。
    #[error("invalid DPI scale factor {factor}: must be finite and greater than zero")]
    InvalidDpiScale { factor: f32 },

    /// 节点句柄不可用：从未存在、已卸载（代际不匹配），或**不是可移除的子节点**（root）。
    #[error("stale node handle {node:?}: the node was unmounted or never existed in this window")]
    StaleNode { node: NodeId },

    /// 派生值形成环（或该派生的闭包上次 panic 未放回）。
    #[error(
        "cyclic derived value {derived:?}: its compute closure depends on itself (directly or \
         through another derived value)"
    )]
    CyclicDerived { derived: DerivedId },

    /// measure 闭包返回了非有限值或负尺寸（设计 §4.8）。
    #[error(
        "invalid measure for {node:?} on the {axis:?} axis: {value} \
         (measure closures must return finite, non-negative sizes)"
    )]
    InvalidMeasure {
        node: NodeId,
        axis: Axis,
        value: f32,
    },

    /// 文本形状失败（设计 §4.8 / P01 §6.2.2 #13）：字体栈为空、行内盒字节偏移非法、超宽降级需上报。
    #[error("text shaping failed for {node:?}: {detail}")]
    TextShape { node: NodeId, detail: SmolStr },

    /// 图集无法容纳新图元（清空重建后仍失败，P02 §10.6 / P01 §6.2.2 #12）。
    #[error("atlas {kind:?} is exhausted: no free tile fits after a clear and rebuild")]
    AtlasExhausted { kind: AtlasKind },

    /// 静态几何注册 / 查找失败（设计 §4.8）。首个生产者是 P08 的渲染层：
    /// `FrameOp::Geometry` 的 `GeometryId` 没有对应的 GPU 缓冲（`GeometryStore` 属 P09）。
    #[error("geometry registration failed: {detail}")]
    GeometryRegister { detail: SmolStr },

    /// 引擎 / 平台初始化失败（`Application::run` 的前置条件；P08 的渲染器初始化也走这里）。
    #[error("engine initialization failed: {detail}")]
    AppInit { detail: SmolStr },

    /// 原生窗口创建 / 消息泵失败（`RegisterClassW` / `CreateWindowExW` / `GetMessageW`）。
    #[error("window creation failed: {detail}")]
    WindowCreate { detail: SmolStr },

    /// 剪贴板访问失败（被其它进程占用 / 内存分配失败）。
    #[error("clipboard access failed: {detail}")]
    Clipboard { detail: SmolStr },
    /// 目标节点不是滚动容器（未 `make_scroll`，或 `Style.clip != Clip::Bounds`）。
    #[error("node {node:?} is not scrollable (make_scroll / Clip::Bounds required)")]
    NotScrollable {
        /// 目标节点。
        node: NodeId,
    },
    /// 目标节点不是虚拟列表容器（未 `make_virtual_list`，或不是 `Clip::Bounds + position: relative`）。
    #[error("node {node:?} is not a virtual list (Clip::Bounds + position: relative required)")]
    NotVirtualList {
        /// 目标节点。
        node: NodeId,
    },
    /// 窗口已关闭 / 从未存在（P10：需要返回句柄或开启动画时无法静默）。
    #[error("unknown window: {window:?}")]
    UnknownWindow {
        /// 陈旧/未知的窗口句柄。
        window: WindowId,
    },
    /// 平台 a11y adapter 初始化失败（必须在窗口**首次显示之前**构造，见 `design/phase-11-a11y.md` §8.1）。
    #[error("accessibility adapter initialization failed: {detail}")]
    A11yInit { detail: SmolStr },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::atlas::AtlasKind;

    fn unsupported_gpu() -> EngineError {
        EngineError::UnsupportedGpu {
            os: "windows",
            required: "FL 12_0 / SM 6.2",
            actual: SmolStr::new_static("FL 11_0 / SM 6_0"),
        }
    }

    fn all_variants() -> Vec<EngineError> {
        vec![
            unsupported_gpu(),
            EngineError::DeviceLost {
                reason: SmolStr::new_static("DXGI_ERROR_DEVICE_REMOVED"),
                log_path: PathBuf::from("logs/dred.txt"),
            },
            EngineError::WaylandCompositor {
                detail: SmolStr::new_static("xdg_wm_base version 3 required"),
            },
            EngineError::ShaderLoad {
                target: "dxil",
                path: PathBuf::from("shaders/quad.dxil"),
            },
            EngineError::Swapchain {
                detail: SmolStr::new_static("CreateSwapChainForHwnd failed"),
            },
            EngineError::InvalidDpiScale { factor: 0.0 },
            EngineError::StaleNode {
                node: NodeId::for_test(3, 1),
            },
            EngineError::CyclicDerived {
                derived: DerivedId::for_test(0),
            },
            EngineError::InvalidMeasure {
                node: NodeId::for_test(4, 1),
                axis: Axis::X,
                value: f32::NAN,
            },
            EngineError::TextShape {
                node: NodeId::for_test(5, 1),
                detail: SmolStr::new_static("font stack is empty"),
            },
            EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph,
            },
            EngineError::GeometryRegister {
                detail: SmolStr::new_static("geometry id 0:0 has no GPU buffers"),
            },
            EngineError::AppInit {
                detail: SmolStr::new_static("d3d12 D3D12CreateDevice: 0x887A0004"),
            },
            EngineError::WindowCreate {
                detail: SmolStr::new_static("CreateWindowExW: 0x8007057E"),
            },
            EngineError::Clipboard {
                detail: SmolStr::new_static("OpenClipboard: 0x80070005"),
            },
        ]
    }

    #[test]
    fn text_shape_display_names_node_and_detail() {
        let text = EngineError::TextShape {
            node: NodeId::for_test(6, 2),
            detail: SmolStr::new_static("inline box index 1 is not a char boundary"),
        }
        .to_string();

        assert!(text.contains("text shaping failed"), "{text}");
        assert!(text.contains("n6.2"), "{text}");
        assert!(text.contains("char boundary"), "{text}");
    }

    #[test]
    fn atlas_exhausted_display_names_the_kind() {
        let text = EngineError::AtlasExhausted {
            kind: AtlasKind::Image,
        }
        .to_string();

        assert!(text.contains("Image"), "{text}");
        assert!(text.contains("exhausted"), "{text}");
        assert!(text.contains("clear and rebuild"), "{text}");
    }

    #[test]
    fn invalid_measure_display_names_node_axis_and_value() {
        let text = EngineError::InvalidMeasure {
            node: NodeId::for_test(2, 1),
            axis: Axis::Y,
            value: -1.5,
        }
        .to_string();

        assert!(text.contains("invalid measure"), "{text}");
        assert!(text.contains("n2.1"), "{text}");
        assert!(text.contains("Y"), "{text}");
        assert!(text.contains("-1.5"), "{text}");
        assert!(text.contains("finite, non-negative"), "{text}");
    }

    #[test]
    fn stale_node_display_mentions_the_state() {
        let text = EngineError::StaleNode {
            node: NodeId::for_test(3, 1),
        }
        .to_string();

        assert!(text.contains("n3.1"), "{text}");
        assert!(text.contains("unmounted or never existed"), "{text}");
    }

    #[test]
    fn cyclic_derived_display_mentions_self_dependency() {
        let text = EngineError::CyclicDerived {
            derived: DerivedId::for_test(2),
        }
        .to_string();

        assert!(text.contains("DerivedId(2)"), "{text}");
        assert!(text.contains("itself"), "{text}");
    }

    #[test]
    fn engine_error_is_send_sync_static() {
        fn assert_send_sync_static<T: Send + Sync + 'static>() {}

        assert_send_sync_static::<EngineError>();
    }

    #[test]
    fn unsupported_gpu_display_lists_required_and_actual() {
        let text = unsupported_gpu().to_string();

        assert!(text.contains("windows"), "{text}");
        assert!(text.contains("FL 12_0 / SM 6.2"), "{text}");
        assert!(text.contains("FL 11_0 / SM 6_0"), "{text}");
        assert!(text.contains("release notes"), "{text}");
    }

    #[test]
    fn unsupported_gpu_display_has_no_fallback_promise() {
        let text = unsupported_gpu().to_string().to_lowercase();

        assert!(!text.contains("software rendering"), "{text}");
        assert!(!text.contains("falling back"), "{text}");
        assert!(!text.contains("try again"), "{text}");
    }

    #[test]
    fn device_lost_display_includes_path() {
        let text = EngineError::DeviceLost {
            reason: SmolStr::new_static("device removed"),
            log_path: PathBuf::from("logs/dred.txt"),
        }
        .to_string();

        assert!(text.contains("device removed"), "{text}");
        assert!(text.contains("logs/dred.txt"), "{text}");
    }

    #[test]
    fn wayland_compositor_display_includes_detail() {
        let text = EngineError::WaylandCompositor {
            detail: SmolStr::new_static("no wl_output"),
        }
        .to_string();

        assert!(text.contains("no wl_output"), "{text}");
    }

    #[test]
    fn shader_load_display_includes_target_and_path() {
        let text = EngineError::ShaderLoad {
            target: "spirv",
            path: PathBuf::from("shaders/quad.spv"),
        }
        .to_string();

        assert!(text.contains("spirv"), "{text}");
        assert!(text.contains("shaders/quad.spv"), "{text}");
    }

    #[test]
    fn swapchain_display_includes_detail() {
        let text = EngineError::Swapchain {
            detail: SmolStr::new_static("resize failed"),
        }
        .to_string();

        assert!(text.contains("resize failed"), "{text}");
    }

    #[test]
    fn invalid_dpi_scale_display_includes_factor() {
        let text = EngineError::InvalidDpiScale { factor: 2.5 }.to_string();

        assert!(text.contains("2.5"), "{text}");
    }

    #[test]
    fn engine_error_source_is_none() {
        for error in all_variants() {
            assert!(
                std::error::Error::source(&error).is_none(),
                "{error:?} unexpectedly has a source"
            );
        }
    }

    #[test]
    fn engine_error_debug_contains_variant_name() {
        let debug = format!("{:?}", unsupported_gpu());

        assert!(debug.contains("UnsupportedGpu"), "{debug}");
    }

    #[test]
    fn engine_error_usable_as_boxed_error() {
        let error: Box<dyn std::error::Error> = Box::new(unsupported_gpu());

        assert!(error.to_string().contains("GPU baseline not met"));
    }
}

//! 引擎公开错误面。
//!
//! 只有强类型变体，不出现字符串错误或通用错误包装（AGENTS §3）。`Display` 为英文诊断文本，
//! 只用于日志 / CI / 开发者；面向用户的本地化文案由应用层按变体生成（i18n 留应用层）。
//!
//! 变体表在 `design/phase-01-foundation.md` §6.2.2 冻结为 17 项。已落地的变体见下方定义；
//! `LayoutInvalid` 无生产者、继续留空。`GeometryRegister` → P09，
//! `AtlasExhausted` / `TextShape` → **P05**（DEVIATIONS F-55），`AppInit` → P08，
//! `WindowCreate` → P12，`A11yInit` → P14，`ResourceDecode` → **P13**（payload 见 DEVIATIONS F-138）。
//! 完整「名称 + payload + 实现 Phase」表见 `design/phase-01-foundation.md` §6.2.2；
//! `StaleNode` / `CyclicDerived` 的实现 Phase 由 P04 改为 P03（`design/DEVIATIONS.md` S-30），
//! `InvalidMeasure` 由 P05 改为 P04（DEVIATIONS F-51）。

use std::path::PathBuf;

use smol_str::SmolStr;

use crate::{
    layout::Axis,
    node::{id::NodeId, signal::DerivedId},
    paint::atlas::AtlasKind,
    resources::DecodeCause,
    window::WindowId,
};

/// Wayland 协议层失败的分类（[`EngineError::Wayland`] 的 `kind`，R10-16）。
///
/// 分类依据是**处理策略**，不是错误码来源：
///
/// | 分类 | 触发者 | 调用方应做什么 |
/// |---|---|---|
/// | [`Self::CompositorGone`] | `EPIPE` / `ENOENT` / 连接被关闭（合成器崩溃、注销、VT 切换） | 提示用户并退出；重连需要重建整条管线 |
/// | [`Self::Protocol`] | `EPROTO` / `BadMessage`（发给未知对象 id 的事件、非法请求序列） | 客户端 bug：写日志 + 上报，不要重试 |
/// | [`Self::MissingGlobal`] | 必需 global 缺失 / 版本不足（`xdg_wm_base` / `wl_compositor` / …） | 环境不支持：提示用户（Linux 不回退 X11，I10） |
/// | [`Self::SurfaceLost`] | 单个 surface / role 对象失效（拿不到首个 configure、事件路由不到窗口） | 关闭该窗口或重建它 —— **不必**杀掉整个进程 |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaylandErrorKind {
    /// 合成器消失 / 连接断开（`EPIPE` / `ENOENT` / 读端 EOF）。
    CompositorGone,
    /// 协议错误（`EPROTO` / `BadMessage`）：客户端违反了协议。
    Protocol,
    /// 缺少必需的 global，或已绑定的版本低于要求。
    MissingGlobal,
    /// 单个 surface / role 对象已失效（窗口级失败）。
    SurfaceLost,
}

impl WaylandErrorKind {
    /// 稳定诊断名（`Display` 用；日志/CI 可 grep）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CompositorGone => "compositor gone",
            Self::Protocol => "protocol error",
            Self::MissingGlobal => "missing global",
            Self::SurfaceLost => "surface lost",
        }
    }

    /// 是否值得重试（只有窗口级的失效可以重建后重试）。
    #[must_use]
    pub const fn is_recoverable(self) -> bool {
        matches!(self, Self::SurfaceLost)
    }

    /// 把底层 IO 错误分类成 [`WaylandErrorKind`]（R10-16）。
    ///
    /// 依据是**处理策略**而不是错误文本：
    ///
    /// - `EPIPE` / `ECONNRESET` / `ENOENT` / `EBADF` ⇒ 连接没了（合成器崩溃、会话注销、
    ///   `WAYLAND_DISPLAY` 指向的 socket 被删）⇒ [`Self::CompositorGone`]；
    /// - `EPROTO` ⇒ 协议错误（客户端发了非法请求，或服务端发来了本端不认识的对象事件）
    ///   ⇒ [`Self::Protocol`]；
    /// - 其余（含 `WouldBlock` 之类的瞬时错误，调用方不该把它们送进来）归
    ///   [`Self::Protocol`]：宁可报「客户端 bug」也不要谎称「合成器还在」。
    #[must_use]
    pub fn from_io_error(error: &std::io::Error) -> Self {
        /// Linux `EPROTO`：libwayland 与 `calloop-wayland-source` 都把它当作协议错误上报。
        const EPROTO: i32 = 71;
        use std::io::ErrorKind;
        if error.raw_os_error() == Some(EPROTO) {
            return Self::Protocol;
        }
        match error.kind() {
            ErrorKind::BrokenPipe
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotFound
            | ErrorKind::UnexpectedEof => Self::CompositorGone,
            _ => Self::Protocol,
        }
    }
}

impl std::fmt::Display for WaylandErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

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

    /// Wayland 协议层失败（R10-16）。
    ///
    /// **按 `kind` 分派**，不要把运行期断连折叠成 [`EngineError::AppInit`]：合成器消失要提示
    /// 「合成器断开，请重新登录会话」，协议错误是本客户端的 bug（需要日志 + 上报），两者对用户
    /// 与对 CI 的意义完全不同。Linux 不回退其它窗口系统（I10）。
    #[error("Wayland {kind}: {detail}")]
    Wayland {
        /// 失败分类（调用方据此决定「退出 / 重连 / 报 bug」）。
        kind: WaylandErrorKind,
        /// 具体诊断（含 libwayland 的原始错误文本 / 缺失的 global 名）。
        detail: SmolStr,
    },

    /// shader 产物加载失败。`target` 为 `"dxil"` / `"metallib"` / `"spirv"`。
    #[error("failed to load shader artifact for {target} at {path}")]
    ShaderLoad { target: &'static str, path: PathBuf },

    /// swapchain / offscreen 重建失败。
    #[error("swapchain reconfiguration failed: {detail}")]
    Swapchain { detail: SmolStr },

    /// DirectComposition 的**运行时**调用失败（visual 几何 / `Commit` / 交换链切换）。
    ///
    /// [`EngineError::AppInit`] 只用于 `Composition::new` 的 COM 创建阶段（窗口还建不出来）；
    /// 首帧 setup 与窗口生命周期内的 `SetContent` / `Commit` 失败都归这里（窗口已经存在，只是
    /// 不能再合成）。`detail` 带失败的操作名。
    #[error("DirectComposition call failed: {detail}")]
    Composition { detail: SmolStr },

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

    /// clip 链超过 `MAX_CLIP_DEPTH`（8 项，`paint/scene.rs`；三套 shader 的固定循环上限）。
    ///
    /// **编码期硬失败**：轴对齐矩形 clip 在编码期求交合并成一条表项（Skia `SkClipStack`
    /// 语义），只有圆角 clip 逐层占项；链长确实超限时 shader 只能少裁，静默丢最外层会让
    /// 子内容越界可见，因此这里明确报错（debug / release 行为一致）。
    #[error(
        "clip chain depth {depth} exceeds the limit {limit} (rect clips are merged at encode time; \
         each rounded clip costs one entry)"
    )]
    ClipDepthExceeded {
        /// 编码期的**圆角层数**（矩形项在编码期被求交合并成一条 scissor，不占预算；
        /// 链长上界 = 该值 + 0/1 条矩形项）。
        depth: u8,
        /// 圆角层数上限（`MAX_CLIP_DEPTH`）。
        limit: u8,
    },

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

    /// 图片资源解码 / 装箱失败（P13：`ResourceCache::insert`）。
    ///
    /// payload 是**强类型分类**（`design/phase-13-resources-snapshot.md` §5.2 / DEVIATIONS F-138）：
    /// 调用方按 `DecodeCause` 决定「占位图 / 报错 / 卸载重试」，不需要解析字符串。
    #[error("image resource decode failed: {cause}")]
    ResourceDecode {
        /// 失败分类。
        cause: DecodeCause,
    },
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
            EngineError::Wayland {
                kind: WaylandErrorKind::MissingGlobal,
                detail: SmolStr::new_static("xdg_wm_base version 3 required"),
            },
            EngineError::ShaderLoad {
                target: "dxil",
                path: PathBuf::from("shaders/quad.dxil"),
            },
            EngineError::Swapchain {
                detail: SmolStr::new_static("CreateSwapChainForHwnd failed"),
            },
            EngineError::Composition {
                detail: SmolStr::new_static("set_geometry: 0x8007000E"),
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
            EngineError::ResourceDecode {
                cause: DecodeCause::Capacity {
                    width: 900,
                    height: 700,
                    page: (2048, 2048),
                    cause: crate::resources::CapacityCause::PageFull,
                },
            },
        ]
    }

    #[test]
    fn resource_decode_display_names_the_cause() {
        let text = EngineError::ResourceDecode {
            cause: DecodeCause::UnsupportedFormat {
                format: SmolStr::new_static("gif"),
            },
        }
        .to_string();

        assert!(text.contains("image resource decode failed"), "{text}");
        assert!(text.contains("unsupported image format `gif`"), "{text}");
        assert!(text.contains("PNG, JPEG and WebP only"), "{text}");
    }

    #[test]
    /// `Capacity` 的文案必须说清「页已满」而不是「页被首图锁死」（P1-2.7 之后页尺寸固定为
    /// 最大档，容量失败只可能是页里放不下更多 tile）。
    fn resource_decode_capacity_display_names_the_full_page() {
        let text = EngineError::ResourceDecode {
            cause: DecodeCause::Capacity {
                width: 900,
                height: 700,
                page: (2048, 2048),
                cause: crate::resources::CapacityCause::PageFull,
            },
        }
        .to_string();

        assert!(text.contains("900x700"), "{text}");
        assert!(text.contains("2048x2048"), "{text}");
        assert!(text.contains("page is already full"), "{text}");
        assert!(
            !text.contains("locked by the first inserted image"),
            "不得再暗示页尺寸被首图锁定（P1-2.7 已修）：{text}"
        );
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
    fn wayland_error_display_names_the_kind_and_detail() {
        let text = EngineError::Wayland {
            kind: WaylandErrorKind::MissingGlobal,
            detail: SmolStr::new_static("xdg_wm_base"),
        }
        .to_string();

        assert!(text.contains("missing global"), "{text}");
        assert!(text.contains("xdg_wm_base"), "{text}");
    }

    /// R10-16：分类必须能区分「合成器消失」与「客户端 bug」—— 处理策略不同。
    #[test]
    fn wayland_error_kinds_are_classified_by_recovery_strategy() {
        assert!(!WaylandErrorKind::CompositorGone.is_recoverable());
        assert!(!WaylandErrorKind::Protocol.is_recoverable());
        assert!(!WaylandErrorKind::MissingGlobal.is_recoverable());
        assert!(
            WaylandErrorKind::SurfaceLost.is_recoverable(),
            "单个 surface 失效可以重建窗口，不必杀进程"
        );
        assert_eq!(WaylandErrorKind::Protocol.as_str(), "protocol error");
    }

    /// R10-16：`calloop` / libwayland 的 IO 失败必须落进正确的分类。
    #[test]
    fn io_errors_map_to_wayland_error_kinds() {
        use std::io::{Error, ErrorKind};

        for kind in [
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::NotFound,
            ErrorKind::UnexpectedEof,
        ] {
            assert_eq!(
                WaylandErrorKind::from_io_error(&Error::new(kind, "gone")),
                WaylandErrorKind::CompositorGone,
                "{kind:?} 表示连接没了"
            );
        }
        // EPROTO（Linux errno 71）：协议错误 ⇒ 客户端 bug，不能报「合成器消失」。
        assert_eq!(
            WaylandErrorKind::from_io_error(&Error::from_raw_os_error(71)),
            WaylandErrorKind::Protocol
        );
        assert_eq!(
            WaylandErrorKind::from_io_error(&Error::new(ErrorKind::InvalidInput, "bad")),
            WaylandErrorKind::Protocol
        );
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
    fn composition_display_names_the_failing_operation() {
        let text = EngineError::Composition {
            detail: SmolStr::new_static("Device::Commit: 0x8007000E"),
        }
        .to_string();

        assert!(text.contains("DirectComposition"), "{text}");
        assert!(text.contains("Device::Commit"), "{text}");
        assert!(text.contains("0x8007000E"), "{text}");
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

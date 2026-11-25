use std::path::PathBuf;

use smol_str::SmolStr;

use crate::{
    layout::Axis,
    math::LogicalPx,
    node::{
        id::NodeId,
        signal::{DerivedId, SignalScope},
    },
    paint::atlas::AtlasKind,
    resources::DecodeCause,
    window::WindowId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaylandErrorKind {
    CompositorGone,
    Protocol,
    MissingGlobal,
    SurfaceLost,
}

impl WaylandErrorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CompositorGone => "compositor gone",
            Self::Protocol => "protocol error",
            Self::MissingGlobal => "missing global",
            Self::SurfaceLost => "surface lost",
        }
    }

    #[must_use]
    pub const fn is_recoverable(self) -> bool {
        matches!(self, Self::SurfaceLost)
    }

    #[must_use]
    pub fn from_io_error(error: &std::io::Error) -> Self {
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

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
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

    #[error("graphics device lost: {reason}; diagnostics written to {log_path}")]
    DeviceLost { reason: SmolStr, log_path: PathBuf },

    #[error("Wayland {kind}: {detail}")]
    Wayland {
        kind: WaylandErrorKind,
        detail: SmolStr,
    },

    #[error("failed to load shader artifact for {target} at {path}")]
    ShaderLoad { target: &'static str, path: PathBuf },

    #[error("swapchain reconfiguration failed: {detail}")]
    Swapchain { detail: SmolStr },

    #[error("DirectComposition call failed: {detail}")]
    Composition { detail: SmolStr },

    #[error("frame pipeline stopped: {detail}")]
    FramePipeline { detail: SmolStr },

    #[error("the application frame hook is already installed")]
    FrameHookInstalled,

    #[error("invalid DPI scale factor {factor}: must be finite and greater than zero")]
    InvalidDpiScale { factor: f32 },

    #[error(
        "stale node handle {node:?}: the node was unmounted or never existed in this window, \
         or cannot be used as a text hit target"
    )]
    StaleNode { node: NodeId },

    #[error(
        "signal scope {scope:?} is still read by live node {reader:?}: it does not belong to the \
         subtree being removed ({node:?}) and releasing it would break that live binding"
    )]
    ScopeOwnershipMismatch {
        node: NodeId,
        scope: SignalScope,
        reader: NodeId,
    },

    #[error(
        "cyclic derived value {derived:?}: its compute closure depends on itself (directly or \
         through another derived value)"
    )]
    CyclicDerived { derived: DerivedId },

    #[error(
        "invalid measure for {node:?} on the {axis:?} axis: {value} \
         (measure closures must return finite, non-negative sizes)"
    )]
    InvalidMeasure {
        node: NodeId,
        axis: Axis,
        value: LogicalPx,
    },

    #[error("invalid node spec for a child of {parent:?}: {detail}")]
    NodeSpecInvalid { parent: NodeId, detail: SmolStr },

    #[error("text shaping failed for {node:?}: {detail}")]
    TextShape { node: NodeId, detail: SmolStr },

    #[error("node {node:?} is not an editable text node")]
    TextEditNotAvailable { node: NodeId },

    #[error("invalid editable text spec for {node:?}: {detail}")]
    TextEditSpecInvalid { node: NodeId, detail: SmolStr },

    #[error("editable text layout failed for {node:?}: {detail}")]
    TextEditLayout { node: NodeId, detail: SmolStr },

    #[error("atlas {kind:?} is exhausted: no free tile fits after a clear and rebuild")]
    AtlasExhausted { kind: AtlasKind },

    #[error(
        "atlas {kind:?} growth to {layers} layers rejected: layer {layer} has no full-page upload"
    )]
    AtlasGrowthIncomplete {
        kind: AtlasKind,
        layer: u16,
        layers: u32,
    },

    #[error(
        "clip chain depth {depth} exceeds the limit {limit} (rect clips are merged at encode time; \
         each rounded clip costs one entry)"
    )]
    ClipDepthExceeded { depth: u8, limit: u8 },

    #[error("geometry registration failed: {detail}")]
    GeometryRegister { detail: SmolStr },

    #[error("unknown geometry handle: {geometry:?}")]
    UnknownGeometry {
        geometry: crate::paint::geometry::GeometryId,
    },

    #[error(
        "geometry {geometry:?} {buffer:?} update range {range:?} is out of bounds \
         (current length {len} bytes)"
    )]
    GeometryRangeOutOfBounds {
        geometry: crate::paint::geometry::GeometryId,
        buffer: crate::paint::geometry::GeometryBuffer,
        range: std::ops::Range<u32>,
        len: u32,
    },

    #[error(
        "geometry {geometry:?} {buffer:?} update range {range:?} is not aligned to {align} bytes"
    )]
    GeometryRangeUnaligned {
        geometry: crate::paint::geometry::GeometryId,
        buffer: crate::paint::geometry::GeometryBuffer,
        range: std::ops::Range<u32>,
        align: u32,
    },

    #[error("geometry {geometry:?} {buffer:?} update carries {bytes} bytes for range {range:?}")]
    GeometryRangeBytes {
        geometry: crate::paint::geometry::GeometryId,
        buffer: crate::paint::geometry::GeometryBuffer,
        range: std::ops::Range<u32>,
        bytes: usize,
    },

    #[error(
        "geometry {geometry:?} {buffer:?} truncate to {requested} elements would grow it \
         (current length {current} elements)"
    )]
    GeometryTruncateGrows {
        geometry: crate::paint::geometry::GeometryId,
        buffer: crate::paint::geometry::GeometryBuffer,
        requested: u32,
        current: u32,
    },

    #[error("engine initialization failed: {detail}")]
    AppInit { detail: SmolStr },

    #[error("window creation failed: {detail}")]
    WindowCreate { detail: SmolStr },

    #[error("clipboard access failed: {detail}")]
    Clipboard { detail: SmolStr },
    #[error("node {node:?} is not scrollable (make_scroll / Clip::Bounds required)")]
    NotScrollable { node: NodeId },
    #[error("node {node:?} is not a virtual list (Clip::Bounds + position: relative required)")]
    NotVirtualList { node: NodeId },

    #[error("virtual list item count mismatch: expected {expected}, got {actual}")]
    VirtualListExtents { expected: usize, actual: usize },

    #[error(
        "virtual list extent at index {index} is invalid: {value} \
         (each extent must be finite and non-negative and their total must be finite)"
    )]
    InvalidVirtualExtent { index: usize, value: LogicalPx },
    #[error("unknown window: {window:?}")]
    UnknownWindow { window: WindowId },
    #[error("accessibility adapter initialization failed: {detail}")]
    A11yInit { detail: SmolStr },

    #[error("image resource decode failed: {cause}")]
    ResourceDecode { cause: DecodeCause },

    #[error(
        "invalid image intrinsic size {width}x{height}: both sides must be finite and greater than \
         zero"
    )]
    InvalidImageIntrinsic { width: f32, height: f32 },

    #[error("window {window:?} has no {capability} on this platform")]
    UnsupportedWindowCapability {
        window: WindowId,
        capability: &'static str,
    },

    #[error("window {window:?} icon decode failed: {cause}")]
    WindowIcon {
        window: WindowId,
        cause: WindowIconCause,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowIconCause {
    UnsupportedFormat,
    DecodeFailed,
    TooLarge { width: u32, height: u32 },
}

impl std::fmt::Display for WindowIconCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat => {
                formatter.write_str("unsupported format (PNG or ICO required)")
            }
            Self::DecodeFailed => formatter.write_str("decode failed"),
            Self::TooLarge { width, height } => write!(
                formatter,
                "decoded size {width}x{height} exceeds the {} px limit",
                crate::window::MAX_WINDOW_ICON_DIM
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{math::px, paint::atlas::AtlasKind};

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
            EngineError::ScopeOwnershipMismatch {
                node: NodeId::for_test(9, 1),
                scope: SignalScope::from_index(2),
                reader: NodeId::for_test(10, 1),
            },
            EngineError::CyclicDerived {
                derived: DerivedId::for_test(0),
            },
            EngineError::InvalidMeasure {
                node: NodeId::for_test(4, 1),
                axis: Axis::X,
                value: px(f32::NAN),
            },
            EngineError::TextShape {
                node: NodeId::for_test(5, 1),
                detail: SmolStr::new_static("font stack is empty"),
            },
            EngineError::TextEditNotAvailable {
                node: NodeId::for_test(11, 1),
            },
            EngineError::TextEditSpecInvalid {
                node: NodeId::for_test(12, 1),
                detail: SmolStr::new_static("masked editable text cannot be multiline"),
            },
            EngineError::TextEditLayout {
                node: NodeId::for_test(13, 1),
                detail: SmolStr::new_static("text has not been shaped yet"),
            },
            EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph,
            },
            EngineError::AtlasGrowthIncomplete {
                kind: AtlasKind::Image,
                layer: 1,
                layers: 2,
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
            EngineError::InvalidImageIntrinsic {
                width: f32::NAN,
                height: -1.0,
            },
            EngineError::UnsupportedWindowCapability {
                window: crate::window::WindowId::for_test(7),
                capability: "fullscreen",
            },
            EngineError::WindowIcon {
                window: crate::window::WindowId::for_test(8),
                cause: WindowIconCause::DecodeFailed,
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
            value: crate::math::px(-1.5),
        }
        .to_string();

        assert!(text.contains("invalid measure"), "{text}");
        assert!(text.contains("n2.1"), "{text}");
        assert!(text.contains("Y"), "{text}");
        assert!(text.contains("-1.5"), "{text}");
        assert!(text.contains("finite, non-negative"), "{text}");
    }

    #[test]
    fn text_edit_errors_name_the_node_and_detail() {
        let unavailable = EngineError::TextEditNotAvailable {
            node: NodeId::for_test(12, 2),
        }
        .to_string();
        assert!(
            unavailable.contains("not an editable text node"),
            "{unavailable}"
        );
        assert!(unavailable.contains("n12.2"), "{unavailable}");

        let invalid = EngineError::TextEditSpecInvalid {
            node: NodeId::for_test(13, 2),
            detail: SmolStr::new_static("masked editable text cannot be multiline"),
        }
        .to_string();
        assert!(invalid.contains("invalid editable text spec"), "{invalid}");
        assert!(invalid.contains("masked"), "{invalid}");

        let layout = EngineError::TextEditLayout {
            node: NodeId::for_test(14, 2),
            detail: SmolStr::new_static("text has not been shaped yet"),
        }
        .to_string();
        assert!(layout.contains("editable text layout failed"), "{layout}");
        assert!(layout.contains("shaped yet"), "{layout}");
    }

    #[test]
    fn stale_node_display_mentions_the_state() {
        let text = EngineError::StaleNode {
            node: NodeId::for_test(3, 1),
        }
        .to_string();

        assert!(text.contains("n3.1"), "{text}");
        assert!(text.contains("unmounted or never existed"), "{text}");
        assert!(
            text.contains("text hit target"),
            "v2.1 E5 复用该变体表示「不能作为文本命中目标」：{text}"
        );
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
    fn scope_ownership_mismatch_display_names_the_live_reader() {
        let text = EngineError::ScopeOwnershipMismatch {
            node: NodeId::for_test(9, 1),
            scope: SignalScope::from_index(2),
            reader: NodeId::for_test(10, 1),
        }
        .to_string();

        assert!(text.contains("live node"), "{text}");
        assert!(text.contains("releasing it would break"), "{text}");
    }

    #[test]
    fn invalid_image_intrinsic_display_names_both_sides() {
        let text = EngineError::InvalidImageIntrinsic {
            width: 0.0,
            height: f32::NAN,
        }
        .to_string();

        assert!(text.contains("invalid image intrinsic"), "{text}");
        assert!(text.contains("0"), "{text}");
        assert!(text.contains("NaN"), "{text}");
        assert!(text.contains("greater than zero"), "{text}");
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
    fn unsupported_window_capability_display_names_window_and_capability() {
        let error = EngineError::UnsupportedWindowCapability {
            window: crate::window::WindowId::for_test(3),
            capability: "fullscreen",
        };
        let text = error.to_string();
        assert!(
            text.contains("WindowId(3)"),
            "window id 必须在文案里：{text}"
        );
        assert!(text.contains("fullscreen"), "{text}");
        assert!(text.contains("no fullscreen"), "{text}");
    }

    #[test]
    fn window_icon_display_names_the_cause() {
        let error = EngineError::WindowIcon {
            window: crate::window::WindowId::for_test(4),
            cause: WindowIconCause::UnsupportedFormat,
        };
        let text = error.to_string();
        assert!(text.contains("WindowId(4)"), "{text}");
        assert!(text.contains("PNG or ICO"), "{text}");

        let too_large = EngineError::WindowIcon {
            window: crate::window::WindowId::for_test(4),
            cause: WindowIconCause::TooLarge {
                width: 2048,
                height: 4096,
            },
        }
        .to_string();
        assert!(too_large.contains("2048x4096"), "{too_large}");
        assert!(too_large.contains("1024"), "{too_large}");
    }

    #[test]
    fn engine_error_usable_as_boxed_error() {
        let error: Box<dyn std::error::Error> = Box::new(unsupported_gpu());

        assert!(error.to_string().contains("GPU baseline not met"));
    }
}

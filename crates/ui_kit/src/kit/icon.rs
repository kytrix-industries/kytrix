use std::fmt;

use smol_str::SmolStr;
use ui_core::{
    a11y::A11yOverride,
    app::App,
    error::EngineError,
    math::{LogicalPx, LogicalSize},
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit},
    paint::{
        color::Color,
        style::{Style, StylePatch},
    },
    resources::ResourceImage,
    window::WindowId,
};

use crate::theme::{handle::ThemeHandle, role::Role};

pub const DEFAULT_ICON_SIZE: LogicalPx = LogicalPx::new(16.0);

pub const ICON_INTRINSIC_SIZE: LogicalSize = LogicalSize::new(DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum IconKind {
    X,
    XBold,
    Circle,
    CircleX,
    RadioButton,
    Square,
    SquareBold,
    CheckSquare,
    Check,
    CheckCircle,
    CircleNotch,
    CaretDown,
    CaretLeft,
    CaretRight,
    ArrowUp,
    ArrowDown,
    ArrowCircleUpFill,
    Eye,
    EyeClosed,
    Minus,
    MinusBold,
    CopySimple,
    CopySimpleBold,
    Plus,
    NotePencil,
    Trash,
    PuzzlePiece,
    SquareHalf,
    SquaresFour,
    Table,
    ColumnsPlusLeft,
    ColumnsPlusRight,
    RowsPlusTop,
    RowsPlusBottom,
    ArrowsClockwise,
    Repeat,
    MagnifyingGlass,
    At,
    Play,
    GearSix,
    WarningCircle,
    User,
    Faders,
    FunnelX,
    Graph,
    Plugs,
    PlugsConnected,
    SpeakerSimpleHigh,
    SpeakerSimpleSlash,
    Sidebar,
    Bank,
    DownloadSimple,
    Pause,
    StopFill,
    Translate,
}

impl IconKind {
    pub const COUNT: usize = 55;

    pub const ALL: [IconKind; Self::COUNT] = [
        IconKind::X,
        IconKind::XBold,
        IconKind::Circle,
        IconKind::CircleX,
        IconKind::RadioButton,
        IconKind::Square,
        IconKind::SquareBold,
        IconKind::CheckSquare,
        IconKind::Check,
        IconKind::CheckCircle,
        IconKind::CircleNotch,
        IconKind::CaretDown,
        IconKind::CaretLeft,
        IconKind::CaretRight,
        IconKind::ArrowUp,
        IconKind::ArrowDown,
        IconKind::ArrowCircleUpFill,
        IconKind::Eye,
        IconKind::EyeClosed,
        IconKind::Minus,
        IconKind::MinusBold,
        IconKind::CopySimple,
        IconKind::CopySimpleBold,
        IconKind::Plus,
        IconKind::NotePencil,
        IconKind::Trash,
        IconKind::PuzzlePiece,
        IconKind::SquareHalf,
        IconKind::SquaresFour,
        IconKind::Table,
        IconKind::ColumnsPlusLeft,
        IconKind::ColumnsPlusRight,
        IconKind::RowsPlusTop,
        IconKind::RowsPlusBottom,
        IconKind::ArrowsClockwise,
        IconKind::Repeat,
        IconKind::MagnifyingGlass,
        IconKind::At,
        IconKind::Play,
        IconKind::GearSix,
        IconKind::WarningCircle,
        IconKind::User,
        IconKind::Faders,
        IconKind::FunnelX,
        IconKind::Graph,
        IconKind::Plugs,
        IconKind::PlugsConnected,
        IconKind::SpeakerSimpleHigh,
        IconKind::SpeakerSimpleSlash,
        IconKind::Sidebar,
        IconKind::Bank,
        IconKind::DownloadSimple,
        IconKind::Pause,
        IconKind::StopFill,
        IconKind::Translate,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            IconKind::X => "x",
            IconKind::XBold => "x-bold",
            IconKind::Circle => "circle",
            IconKind::CircleX => "x-circle",
            IconKind::RadioButton => "radio-button",
            IconKind::Square => "square",
            IconKind::SquareBold => "square-bold",
            IconKind::CheckSquare => "check-square",
            IconKind::Check => "check",
            IconKind::CheckCircle => "check-circle",
            IconKind::CircleNotch => "circle-notch",
            IconKind::CaretDown => "caret-down",
            IconKind::CaretLeft => "caret-left",
            IconKind::CaretRight => "caret-right",
            IconKind::ArrowUp => "arrow-up",
            IconKind::ArrowDown => "arrow-down",
            IconKind::ArrowCircleUpFill => "arrow-circle-up-fill",
            IconKind::Eye => "eye",
            IconKind::EyeClosed => "eye-closed",
            IconKind::Minus => "minus",
            IconKind::MinusBold => "minus-bold",
            IconKind::CopySimple => "copy-simple",
            IconKind::CopySimpleBold => "copy-simple-bold",
            IconKind::Plus => "plus",
            IconKind::NotePencil => "note-pencil",
            IconKind::Trash => "trash",
            IconKind::PuzzlePiece => "puzzle-piece",
            IconKind::SquareHalf => "square-half",
            IconKind::SquaresFour => "squares-four",
            IconKind::Table => "table",
            IconKind::ColumnsPlusLeft => "columns-plus-left",
            IconKind::ColumnsPlusRight => "columns-plus-right",
            IconKind::RowsPlusTop => "rows-plus-top",
            IconKind::RowsPlusBottom => "rows-plus-bottom",
            IconKind::ArrowsClockwise => "arrows-clockwise",
            IconKind::Repeat => "repeat",
            IconKind::MagnifyingGlass => "magnifying-glass",
            IconKind::At => "at",
            IconKind::Play => "play",
            IconKind::GearSix => "gear-six",
            IconKind::WarningCircle => "warning-circle",
            IconKind::User => "user",
            IconKind::Faders => "faders",
            IconKind::FunnelX => "funnel-x",
            IconKind::Graph => "graph",
            IconKind::Plugs => "plugs",
            IconKind::PlugsConnected => "plugs-connected",
            IconKind::SpeakerSimpleHigh => "speaker-simple-high",
            IconKind::SpeakerSimpleSlash => "speaker-simple-slash",
            IconKind::Sidebar => "sidebar",
            IconKind::Bank => "bank",
            IconKind::DownloadSimple => "download-simple",
            IconKind::Pause => "pause",
            IconKind::StopFill => "stop-fill",
            IconKind::Translate => "translate",
        }
    }

    pub fn from_name(name: &str) -> Result<Self, IconError> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.name() == name)
            .ok_or_else(|| IconError::UnknownKind {
                name: SmolStr::new(name),
            })
    }
}

macro_rules! svg_table {
    ($($kind:ident => $stem:literal),* $(,)?) => {
        #[must_use]
        pub fn svg(kind: IconKind) -> &'static str {
            match kind {
                $(
                    IconKind::$kind => {
                        include_str!(concat!(env!("OUT_DIR"), "/icons/", $stem, ".svg"))
                    }
                )*
            }
        }
    };
}

svg_table! {
    X => "x",
    XBold => "x-bold",
    Circle => "circle",
    CircleX => "x-circle",
    RadioButton => "radio-button",
    Square => "square",
    SquareBold => "square-bold",
    CheckSquare => "check-square",
    Check => "check",
    CheckCircle => "check-circle",
    CircleNotch => "circle-notch",
    CaretDown => "caret-down",
    CaretLeft => "caret-left",
    CaretRight => "caret-right",
    ArrowUp => "arrow-up",
    ArrowDown => "arrow-down",
    ArrowCircleUpFill => "arrow-circle-up-fill",
    Eye => "eye",
    EyeClosed => "eye-closed",
    Minus => "minus",
    MinusBold => "minus-bold",
    CopySimple => "copy-simple",
    CopySimpleBold => "copy-simple-bold",
    Plus => "plus",
    NotePencil => "note-pencil",
    Trash => "trash",
    PuzzlePiece => "puzzle-piece",
    SquareHalf => "square-half",
    SquaresFour => "squares-four",
    Table => "table",
    ColumnsPlusLeft => "columns-plus-left",
    ColumnsPlusRight => "columns-plus-right",
    RowsPlusTop => "rows-plus-top",
    RowsPlusBottom => "rows-plus-bottom",
    ArrowsClockwise => "arrows-clockwise",
    Repeat => "repeat",
    MagnifyingGlass => "magnifying-glass",
    At => "at",
    Play => "play",
    GearSix => "gear-six",
    WarningCircle => "warning-circle",
    User => "user",
    Faders => "faders",
    FunnelX => "funnel-x",
    Graph => "graph",
    Plugs => "plugs",
    PlugsConnected => "plugs-connected",
    SpeakerSimpleHigh => "speaker-simple-high",
    SpeakerSimpleSlash => "speaker-simple-slash",
    Sidebar => "sidebar",
    Bank => "bank",
    DownloadSimple => "download-simple",
    Pause => "pause",
    StopFill => "stop-fill",
    Translate => "translate",
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IconSpec {
    pub kind: IconKind,
    pub size: LogicalPx,
    pub color: Color,
}

pub type Icon = IconSpec;

impl IconSpec {
    #[must_use]
    pub const fn new(kind: IconKind) -> Self {
        Self {
            kind,
            size: DEFAULT_ICON_SIZE,
            color: Color::WHITE,
        }
    }

    #[must_use]
    pub const fn with_size(mut self, size: LogicalPx) -> Self {
        self.size = size;
        self
    }

    #[must_use]
    pub const fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    pub fn build(
        self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, IconError> {
        if !self.size.is_finite() || self.size <= LogicalPx::ZERO {
            return Err(IconError::InvalidSize { size: self.size });
        }
        let resource = self.resource_for(app, window)?;
        let mut style = Style::default();
        style.size_length(self.size, self.size);
        NodeBuilder::image(resource.id(), ImageFit::Fill)
            .image_modulation(self.color)
            .style(style)
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(IconError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )
            .map_err(IconError::Node)
    }

    pub fn build_themed(
        self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        theme: &ThemeHandle,
        role: Role,
    ) -> Result<NodeId, IconError> {
        if !self.size.is_finite() || self.size <= LogicalPx::ZERO {
            return Err(IconError::InvalidSize { size: self.size });
        }
        let resource = self.resource_for(app, window)?;
        let initial = theme.theme_for(theme.current(app)).role(role);
        let mut style = Style::default();
        style.size_length(self.size, self.size);
        let style_theme = theme.clone();
        NodeBuilder::image(resource.id(), ImageFit::Fill)
            .image_modulation(initial)
            .style(style)
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .bind_style(move |cx| StylePatch {
                foreground: Some(style_theme.role(cx, role)),
                ..StylePatch::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(IconError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )
            .map_err(IconError::Node)
    }

    fn resource_for(self, app: &mut App, window: WindowId) -> Result<ResourceImage, IconError> {
        app.insert_svg_for_window(window, svg(self.kind).as_bytes(), ICON_INTRINSIC_SIZE)
            .map_err(|error| match error {
                EngineError::UnknownWindow { window } => IconError::UnknownWindow { window },
                other => IconError::Decode(other),
            })
    }
}

#[derive(Debug)]
pub enum IconError {
    UnknownWindow { window: WindowId },
    InvalidSize { size: LogicalPx },
    UnknownKind { name: SmolStr },
    Decode(EngineError),
    Node(EngineError),
}

impl fmt::Display for IconError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IconError::UnknownWindow { window } => {
                write!(formatter, "icon target window {window:?} is closed")
            }
            IconError::InvalidSize { size } => {
                write!(formatter, "icon size {size} must be finite and > 0")
            }
            IconError::UnknownKind { name } => {
                write!(
                    formatter,
                    "unknown icon kind `{name}` (not a Phosphor icon name)"
                )
            }
            IconError::Decode(error) => write!(formatter, "icon rasterization failed: {error}"),
            IconError::Node(error) => write!(formatter, "icon node mount failed: {error}"),
        }
    }
}

impl std::error::Error for IconError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IconError::Decode(error) | IconError::Node(error) => Some(error),
            IconError::UnknownWindow { .. }
            | IconError::InvalidSize { .. }
            | IconError::UnknownKind { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{
        a11y::{A11yDiagnostic, A11yDrawKind, A11yRole},
        math::{DevicePixelRatio, DeviceSize, LogicalRect, LogicalSize, dp, px},
        node::{
            id::NodeId,
            image::{ImageFit, ImageSource},
            tree::NodeKind,
        },
        paint::color::Color,
        resources::ResourceImage,
        test_support::app::TestApp,
    };

    use super::{DEFAULT_ICON_SIZE, ICON_INTRINSIC_SIZE, IconError, IconKind, IconSpec, svg};
    use crate::{
        kit::test_support::{bounds, image_primitives, test_app, test_app_with_scale},
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
    };

    include!(concat!(env!("OUT_DIR"), "/icons/built.rs"));

    #[test]
    fn every_icon_variant_has_a_unique_name_and_a_built_svg() {
        assert_eq!(
            IconKind::ALL.len(),
            IconKind::COUNT,
            "ALL 必须覆盖全部 55 个变体（少一项意味着某个图标在界面里画不出来）"
        );
        let mut names: Vec<&'static str> = IconKind::ALL.iter().map(|kind| kind.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), IconKind::COUNT, "图标名必须唯一");

        assert_eq!(
            BUILT_ICON_FILES.len(),
            IconKind::COUNT,
            "构建期生成的 SVG 数必须等于枚举数：{BUILT_ICON_FILES:?}"
        );
        for kind in IconKind::ALL {
            assert!(
                BUILT_ICON_FILES.contains(&kind.name()),
                "{kind:?}（{}）没有构建期产物",
                kind.name()
            );
            let svg_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/icons/phosphoricons")
                .join(format!("{}.svg", kind.name()));
            assert!(
                svg_path.is_file(),
                "{kind:?} 的源 SVG `{}` 不存在（映射写错了名字？）",
                svg_path.display()
            );
        }
        for kind in IconKind::ALL {
            assert_eq!(
                IconKind::from_name(kind.name()).expect("名字必须能反查"),
                kind
            );
        }
    }

    #[test]
    fn every_icon_svg_is_a_white_template_without_strokes_or_gradients() {
        for kind in IconKind::ALL {
            let source = svg(kind);
            assert!(source.starts_with("<svg"), "{kind:?} 的模板不是 SVG 文本");
            assert!(
                source.contains("fill=\"#FFFFFF\""),
                "{kind:?} 的模板必须是白色字形（运行期靠 modulation 染色）"
            );
            for forbidden in ["fill=\"#000000\"", "stroke=", "fill=\"url(", "<image"] {
                assert!(
                    !source.contains(forbidden),
                    "{kind:?} 的模板含 `{forbidden}`：构建期应该已经拒绝"
                );
            }
        }
    }

    #[test]
    fn an_icon_registers_one_vector_resource_at_the_design_size() {
        let mut app = test_app_with_scale(1.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let sizes = [8.0f32, 12.0, 16.0, 32.0];
        let nodes: Vec<NodeId> = sizes
            .into_iter()
            .map(|side| {
                IconSpec::new(IconKind::GearSix)
                    .with_size(px(side))
                    .build(app.app_mut(), window, root)
                    .expect("图标挂载")
            })
            .collect();
        let _ = app.drain().expect("首帧");

        let resources: Vec<ResourceImage> = nodes
            .iter()
            .map(|node| resource(&app, window, *node))
            .collect();
        assert!(
            resources.iter().all(|image| *image == resources[0]),
            "同一图标的显示尺寸不进资源身份：{resources:?}"
        );
        assert_eq!(
            resources[0].intrinsic(DevicePixelRatio::ONE),
            ICON_INTRINSIC_SIZE,
            "SVG 登记的是图标内在逻辑尺寸，不是显示目标"
        );
        for (node, side) in nodes.iter().zip(sizes) {
            assert_eq!(
                bounds(&app, *node).map(|rect| rect.size),
                Some(LogicalSize::new(px(side), px(side))),
                "{side} px 图标的布局盒必须等于显式尺寸"
            );
        }
        assert_eq!(
            image_primitives(&app).len(),
            nodes.len(),
            "每个图标仍然各画一个图元"
        );
    }

    #[test]
    fn an_icon_mounts_one_image_primitive_with_the_modulation_color() {
        let mut app = test_app_with_scale(1.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let color = Color::from_rgba8([0x33, 0x99, 0xFF, 0xFF]);

        let node = IconSpec::new(IconKind::CheckCircle)
            .with_size(px(16.0))
            .with_color(color)
            .build(app.app_mut(), window, root)
            .expect("图标挂载");
        let _ = app.drain().expect("首帧");

        assert_eq!(
            bounds(&app, node),
            Some(LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0))),
            "图片节点的布局盒必须正好是显式 Style 尺寸"
        );
        {
            let tree = app.app().window(window).expect("test window").tree();
            let icon = tree.get(node).expect("图标节点");
            match icon.kind() {
                NodeKind::Image(content) => {
                    assert_eq!(content.fit(), ImageFit::Fill, "图标用 Fill 到逻辑尺寸");
                    assert_eq!(content.modulation(), color, "调制色写在 E7 载荷里");
                    assert!(
                        matches!(content.source(), ImageSource::Ready(_)),
                        "图标是同步插入的就绪资源：{:?}",
                        content.source()
                    );
                }
                other => panic!("图标必须是 E7 图片节点，实际 {other:?}"),
            }
            assert!(icon.children().is_empty(), "图标是叶子，不能挂子节点");
        }
        let images = image_primitives(&app);
        assert_eq!(
            images.len(),
            1,
            "一个图标只应产生一个 Image 图元：{images:?}"
        );
        assert_eq!(images[0].modulation, color, "调制色必须是 IconSpec.color");
        assert_eq!(
            images[0].bounds,
            LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)),
            "Fill 图元必须落在节点盒里"
        );
    }

    #[test]
    fn a_dpi_change_keeps_the_intrinsic_and_the_engine_keeps_every_raster() {
        let mut app = test_app_with_scale(1.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let node = IconSpec::new(IconKind::GearSix)
            .with_size(px(16.0))
            .build(app.app_mut(), window, root)
            .expect("图标挂载");
        let _ = app.drain().expect("1× 首帧");
        let image = resource(&app, window, node);
        assert_eq!(
            image.intrinsic(DevicePixelRatio::ONE),
            ICON_INTRINSIC_SIZE,
            "1× 的内在逻辑尺寸是图标设计尺寸"
        );
        let at_1x = app
            .app()
            .resources()
            .variant_for(image, DeviceSize::new(dp(16), dp(16)))
            .expect("1× 帧必须栅格出 16 物理像素变体");

        set_window_scale(&mut app, 2.0);
        let _ = app.drain().expect("2× 首帧");
        assert_eq!(
            image.intrinsic(DevicePixelRatio::new(2.0).expect("2×")),
            ICON_INTRINSIC_SIZE,
            "矢量资源的内在逻辑尺寸不随 DPR 变化"
        );
        assert_eq!(
            bounds(&app, node),
            Some(LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0))),
            "显式 Style 尺寸：DPI 变化不得让图标布局跳动"
        );
        let at_2x = app
            .app()
            .resources()
            .variant_for(image, DeviceSize::new(dp(32), dp(32)))
            .expect("DPI 变化后引擎必须自动栅格出 32 物理像素变体");
        assert_ne!(at_1x, at_2x, "两个物理尺寸必须是两张不同的图集变体");
        assert_eq!(image_primitives(&app).len(), 1, "换变体不得复制图元");
    }

    #[test]
    fn unknown_names_and_invalid_sizes_are_typed_errors() {
        let error = IconKind::from_name("no-such-icon").expect_err("未知名字必须失败");
        assert!(matches!(
            &error,
            IconError::UnknownKind { name } if name == "no-such-icon"
        ));
        let mut app = test_app_with_scale(1.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let side = px(size);
            let error = IconSpec::new(IconKind::X)
                .with_size(side)
                .build(app.app_mut(), window, root)
                .expect_err("非法尺寸必须失败");
            assert!(
                matches!(&error, IconError::InvalidSize { size: actual } if actual.value().to_bits() == size.to_bits()),
                "size={size} 的错误分类不对：{error:?}"
            );
        }
        assert_eq!(DEFAULT_ICON_SIZE, px(16.0), "默认尺寸是旧用法里的常见值");
    }

    #[test]
    fn a_closed_window_is_a_typed_error() {
        let mut app = test_app_with_scale(1.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        assert!(app.app_mut().close_window(window), "窗口必须存在");
        let error = IconSpec::new(IconKind::X)
            .build(app.app_mut(), window, root)
            .expect_err("窗口已关闭");
        assert!(matches!(error, IconError::UnknownWindow { window: closed } if closed == window));
    }

    #[test]
    fn icon_nodes_are_hidden_from_the_a11y_tree() {
        let mut app = test_app();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let node = IconSpec::new(IconKind::CheckCircle)
            .with_size(px(16.0))
            .build(app.app_mut(), window, root)
            .expect("图标挂载");
        let _ = app.drain().expect("首帧");

        let snapshot = app.a11y().expect("a11y 快照（开启捕获后的首帧是全树）");
        let record = snapshot.node_for(node).expect("图片节点必须在 a11y 树里");
        assert_eq!(record.role, A11yRole::Image, "E7 图片节点的默认角色");
        assert!(record.hidden, "图标节点必须标记 hidden：{record:?}");
        assert!(
            !snapshot
                .visible_nodes_in_batch()
                .any(|visible| visible.engine == node),
            "图标节点不得出现在可见 a11y 视图里"
        );

        let diagnostics = app.app().a11y_diagnostics();
        assert!(
            !diagnostics.iter().any(|diagnostic| matches!(
                diagnostic,
                A11yDiagnostic::MissingAltText {
                    node: target,
                    kind: A11yDrawKind::Image
                } if *target == node
            )),
            "显式装饰的图标不得产生 MissingAltText 诊断：{diagnostics:?}"
        );
    }

    #[test]
    fn icons_are_first_class_image_nodes_without_text_or_inline_box_residue() {
        let mut app = test_app();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let sizes = [8.0f32, 12.0, 16.0];
        let nodes: Vec<NodeId> = sizes
            .iter()
            .map(|size| {
                IconSpec::new(IconKind::CaretDown)
                    .with_size(px(*size))
                    .build(app.app_mut(), window, root)
                    .expect("图标挂载")
            })
            .collect();
        let _ = app.drain().expect("首帧");

        let primitives = image_primitives(&app);
        assert_eq!(primitives.len(), sizes.len(), "每个图标一个图元");
        for (node, size) in nodes.iter().zip(sizes) {
            let node_bounds = bounds(&app, *node).expect("图标布局盒");
            assert_eq!(
                node_bounds.size,
                LogicalSize::new(px(size), px(size)),
                "{size} px 图标的布局盒必须等于显式尺寸"
            );
            let image = primitives
                .iter()
                .find(|image| image.bounds == node_bounds)
                .unwrap_or_else(|| panic!("{size} px 图标没有落在节点盒里的图元：{primitives:?}"));
            assert_eq!(image.modulation, Color::WHITE, "默认不染色");
        }

        let tree = app.app().window(window).expect("test window").tree();
        let mut stack = vec![root];
        let mut text_nodes = 0usize;
        while let Some(id) = stack.pop() {
            let node = tree.get(id).expect("树里的节点必须存活");
            if matches!(node.kind(), NodeKind::Text(_)) {
                text_nodes += 1;
            }
            stack.extend(node.children().iter().copied());
        }
        assert_eq!(text_nodes, 0, "图标树里不得残留占位空格文本节点");
    }

    fn set_window_scale(app: &mut TestApp, scale: f32) {
        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("test window")
            .set_device_pixel_ratio(DevicePixelRatio::new(scale).expect("测试 DPI 必须是有限正数"));
    }

    fn resource(
        app: &TestApp,
        window: ui_core::window::WindowId,
        node: NodeId,
    ) -> ui_core::resources::ResourceImage {
        let tree = app.app().window(window).expect("test window").tree();
        let NodeKind::Image(content) = tree.get(node).expect("图标节点").kind() else {
            panic!("图标必须是图片节点");
        };
        let id = content.source().resource().expect("图标必须是就绪资源");
        app.app().resources().image(id).expect("资源必须仍然存活")
    }

    fn resolved(color: Color) -> [f32; 4] {
        ui_core::paint::color::ColorResolver
            .resolve(color)
            .components
    }

    #[test]
    fn a_themed_icon_follows_the_appearance_and_a_static_icon_does_not() {
        let mut app = test_app_with_scale(1.0);
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();

        let themed = IconSpec::new(IconKind::CheckCircle)
            .with_size(px(16.0))
            .build_themed(app.app_mut(), window, root, &theme, Role::FgMuted)
            .expect("主题图标");
        let static_icon = IconSpec::new(IconKind::GearSix)
            .with_size(px(16.0))
            .with_color(Color::from_rgba8([0xFF, 0x00, 0x00, 0xFF]))
            .build(app.app_mut(), window, root)
            .expect("静态图标");
        let _ = app.drain().expect("首帧");

        let light = Theme::of(Appearance::Light);
        let light_muted = resolved(light.role(Role::FgMuted));
        let images = image_primitives(&app);
        assert_eq!(
            images
                .iter()
                .find(|image| image.bounds.origin.x == px(0.0))
                .map(|image| resolved(image.modulation)),
            Some(light_muted),
            "首帧：主题图标的调制色 = 浅色 FgMuted"
        );

        assert!(theme.set_appearance(app.app_mut(), Appearance::Dark));
        let record = app.drain().expect("切深色").clone();
        assert!(
            record.dirty_nodes.contains(&themed),
            "读主题的图标必须被失效：{:?}",
            record.dirty_nodes
        );
        assert!(
            !record.dirty_nodes.contains(&static_icon),
            "静态调制色的图标不得被主题切换失效：{:?}",
            record.dirty_nodes
        );

        let dark_muted = resolved(Theme::of(Appearance::Dark).role(Role::FgMuted));
        let images = image_primitives(&app);
        let themed_modulation = images
            .iter()
            .find(|image| image.bounds.origin.x == px(0.0))
            .map(|image| resolved(image.modulation));
        assert_eq!(themed_modulation, Some(dark_muted), "深色主题图标的调制色");
        assert_ne!(
            light_muted, dark_muted,
            "两套外观的 FgMuted 必须不同（否则上面的断言无意义）"
        );
        let static_modulation = images
            .iter()
            .find(|image| image.bounds.origin.x > px(0.0))
            .map(|image| resolved(image.modulation));
        assert_eq!(
            static_modulation,
            Some(resolved(Color::from_rgba8([0xFF, 0x00, 0x00, 0xFF]))),
            "静态图标的调制色不随主题变化"
        );
    }
}

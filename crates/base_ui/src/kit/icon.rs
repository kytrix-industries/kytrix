//! 图标：Phosphor 字形的枚举、构建期 PNG 与引擎图片节点挂载。
//!
//! # 数据流
//!
//! ```text
//! assets/icons/phosphoricons/<stem>.svg   （构建期，crates/base_ui/build.rs）
//!   ├─ 根元素 fill="#000000" → "#FFFFFF"（白色字形）
//!   └─ resvg 光栅化 ──▶ OUT_DIR/icons/<stem>-48.png / <stem>-96.png（直通 alpha RGBA8，
//!                       white matte：含 alpha=0 的像素也写 RGB=255）
//!
//! 运行期：IconKind ──name()──▶ <stem> ──raster(tier)──▶ &'static [u8]
//!          ── Window::scale() 选档 ──▶ ResourceCache::insert_owned_at_size(逻辑尺寸 × scale)
//!          ── ResourceId ──▶ InlineBoxPaint::Image { resource, modulation: color }
//!          ──▶ Primitive::Image（引擎在 paint 期解析 tile 并乘调制色）
//! ```
//!
//! # 颜色调制
//!
//! 旧实现是 `svg().path(..).text_color(..)`：GPUI 渲染 SVG 时把 `currentColor` 换成文本色。
//! 引擎的图片通道只有 `Primitive::Image.modulation`（`texel * modulation`，预乘空间），所以
//! 图标烘成**白色**字形，运行期 `Color` 就是最终颜色；`Color::WHITE` = 原样（白色），
//! `Color::TRANSPARENT` = 不画（paint 跳过透明调制）。
//!
//! # 档位选择
//!
//! 两档 48 / 96 px，按**目标窗口的 DPI** 选（`Window::scale()`）：
//!
//! | 窗口 scale | 档位 | 典型逻辑尺寸 → 物理目标 |
//! |---|---|---|
//! | `<= 1.0` | 48 px | 16 → 16（48→16 下采样，3× 余量） |
//! | `> 1.0` | 96 px | 16 → 32（96→32 下采样，3× 余量） |
//!
//! 旧实现的图标逻辑尺寸只有 8 / 12 / 16 px（`crate::size::UnifiedSize::icon_size` 与
//! `.size_4()`；统计见 spec-delta §2），所以两档足够覆盖 1× / 2× 且图集不会无谓膨胀。
//! **DPI 变化不会自动换档**：同一节点的 `ResourceId` 是挂载期定的变体，换屏后需要重新挂载
//! 图标节点（引擎的 DPI 失效只重排不重解图片，见 spec-delta §6 遗留项）。
//!
//! # 错误语义
//!
//! `IconKind` 是编译期完备的枚举，**不存在**未知变体；「未知 kind」只出现在按名字反查
//! （[`IconKind::from_name`]：接受 [`IconKind::name()`] 的文件 stem，供 markdown / 配置数据用），
//! 失败是强类型 [`IconError::UnknownKind`]。注意它**不**接受旧 `IconKind::path()` 的全路径
//! （`icons/phosphoricons/x.svg`）：路径是旧 GPUI 资产系统的寻址方式，新实现按 stem 取构建期
//! PNG；从旧配置迁字符串时需要在调用点取文件名（见 spec-delta §6 第 2 条）。挂载 / 解码失败
//! 保留引擎的强类型 `EngineError`。

use std::fmt;

use smol_str::SmolStr;
use ui_core::{
    app::App, error::EngineError, math::Size, node::id::NodeId, paint::color::Color,
    window::WindowId,
};

use crate::kit::image_node;

/// 构造图标时的默认逻辑边长（旧用法里最常见的 16 px；`UnifiedSize::icon_size` 的 Large 档）。
pub const DEFAULT_ICON_SIZE: f32 = 16.0;

/// 一个 Phosphor 图标（55 个，顺序与旧 `crate::icon::IconKind` 一致）。
///
/// 名字就是旧 `IconKind::path()` 里的文件名 stem（`x.svg` → `x`、`x-bold.svg` → `x-bold`），
/// 构建期清单与枚举的一一对应由测试锁定（`build.rs` 写 `OUT_DIR/icons/built.rs`）。
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
    /// 变体总数（= 旧 `IconKind` 的 55 个 = `assets/icons/phosphoricons/*.svg` 的文件数）。
    ///
    /// 用途：`ALL` 少一项 / 资产多一个文件时测试必须变红；它也是 `Icon` 映射「逐项不遗漏」的
    /// 机器证据（设计文档写 54，实际 55，见 spec-delta §1）。
    pub const COUNT: usize = 55;

    /// 全部变体（顺序 = 枚举声明顺序 = 旧 `IconKind::path()` 的匹配顺序）。
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

    /// 图标名（= 构建期产物与源 SVG 共用的文件 stem）。稳定标识：配置 / markdown 用它反查。
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

    /// 按名字反查：接受 [`IconKind::name()`] 的**文件 stem**（`"x"` / `"caret-down"`），
    /// **不**接受旧 `IconKind::path()` 的 `icons/phosphoricons/x.svg` 全路径（那些调用点需要
    /// 先取文件名 stem，见模块文档）。
    ///
    /// # Errors
    /// [`IconError::UnknownKind`]：名字不在 [`IconKind::ALL`] 里。
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

/// 图标光栅的两档边长（物理像素）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RasterTier {
    /// 48 px：1× 窗口（16 px 图标有 3× 下采样余量）。
    Small48,
    /// 96 px：2× 及以上窗口（16 px 图标 @2× = 32 px，有 3× 余量）。
    Large96,
}

impl RasterTier {
    /// 两档（测试 / 清单遍历用）。
    pub const ALL: [RasterTier; 2] = [RasterTier::Small48, RasterTier::Large96];

    /// 边长（物理像素）。
    #[must_use]
    pub const fn pixels(self) -> u32 {
        match self {
            RasterTier::Small48 => 48,
            RasterTier::Large96 => 96,
        }
    }

    /// 按窗口 DPI 选档：`scale > 1.0`（任何高 DPI）用 96 档，否则 48 档。
    ///
    /// 防御：非有限 / 非正的 scale 按 1× 处理（平台回报异常的兜底不改变绘制语义；
    /// 越界的 DPI 本身由引擎在窗口层拒绝）。
    #[must_use]
    pub fn for_scale(scale: f32) -> Self {
        if scale.is_finite() && scale > 1.0 {
            RasterTier::Large96
        } else {
            RasterTier::Small48
        }
    }
}

/// 构建期生成的两档 PNG 表：`IconKind → (48 px, 96 px)` 的 `&'static [u8]`。
///
/// 生成规则见 `crates/base_ui/build.rs`：`OUT_DIR/icons/<stem>-<tier>.png`。表项与源文件一一
/// 对应，缺文件是**编译错误**（`include_bytes!` 解析失败），不会漏到运行期。
macro_rules! raster_table {
    ($($kind:ident => $stem:literal),* $(,)?) => {
        /// 图标 PNG 字节（`RasterTier` 两档之一）。
        ///
        /// 直接进 `ResourceCache::insert*`（PNG/JPEG/WebP 是引擎图片通道的全部输入格式）。
        #[must_use]
        pub fn raster(kind: IconKind, tier: RasterTier) -> &'static [u8] {
            match (kind, tier) {
                $(
                    (IconKind::$kind, RasterTier::Small48) => {
                        include_bytes!(concat!(env!("OUT_DIR"), "/icons/", $stem, "-48.png"))
                    }
                    (IconKind::$kind, RasterTier::Large96) => {
                        include_bytes!(concat!(env!("OUT_DIR"), "/icons/", $stem, "-96.png"))
                    }
                )*
            }
        }
    };
}

raster_table! {
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

/// 一个图标的完整绘制声明（尺寸 + 调制色）。
///
/// 字段公开：组件 builder 可以把它当值存起来（`IconSpec` 是「图标的一句话」，不含资源句柄，
/// 因此可在主题切换 / 重建时重复挂载而不会持有失效的 `AtlasTile`）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IconSpec {
    /// 画哪个字形。
    pub kind: IconKind,
    /// 逻辑边长（正方形；引擎按窗口 DPI 取物理变体）。
    pub size: f32,
    /// 调制色（白色字形 ⇒ 最终颜色）。`Color::TRANSPARENT` = 不画。
    pub color: Color,
}

/// 旧名字：`kit::icon::Icon`（任务 / 设计里用 `IconSpec{kind,size,color}` 与 `Icon::build` 两种写法）。
pub type Icon = IconSpec;

impl IconSpec {
    /// 默认 16 px、白色（调用方通常立刻覆盖颜色；白字白底不可见，所以**不要**省 `.with_color`）。
    #[must_use]
    pub const fn new(kind: IconKind) -> Self {
        Self {
            kind,
            size: DEFAULT_ICON_SIZE,
            color: Color::WHITE,
        }
    }

    /// 设置逻辑边长（正方形）。
    #[must_use]
    pub const fn with_size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    /// 设置调制色。
    #[must_use]
    pub const fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    /// 把图标挂到 `parent` 末尾，返回图片节点句柄。
    ///
    /// 语义：
    /// - 按**窗口 DPI** 选 PNG 档位，并把「逻辑尺寸 × scale」作为物理目标取图集变体
    ///   （`ResourceCache::insert_owned_at_size`：不放大、等比 fit；同 内容+变体 只占一个 tile）；
    /// - 资源引用登记在窗口名下（关窗统一回收），重复挂载同一图标只增引用计数；
    /// - 节点是**参与布局的图片节点**（空文本 + 行内盒图片，见 `kit::image_node` 的模块文档），
    ///   颜色经 `InlineBoxPaint::Image.modulation` 在 paint 期调制。
    ///
    /// # Errors
    /// - [`IconError::UnknownWindow`]：窗口已关闭；
    /// - [`IconError::InvalidSize`]：`size` 非有限或 `<= 0`；
    /// - [`IconError::Decode`]：PNG 解码 / 图集装箱失败（引擎强类型 `EngineError`）；
    /// - [`IconError::Node`]：父节点陈旧 / 布局叶子等建树错误（引擎强类型 `EngineError`）。
    pub fn build(
        self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, IconError> {
        if !self.size.is_finite() || self.size <= 0.0 {
            return Err(IconError::InvalidSize { size: self.size });
        }
        let scale = app
            .window(window)
            .ok_or(IconError::UnknownWindow { window })?
            .scale()
            .factor();
        let tier = RasterTier::for_scale(scale);
        let physical = Size::new(self.size * scale, self.size * scale);
        let resource = app
            .resources_mut()
            .insert_owned_at_size(window, raster(self.kind, tier), physical)
            .map_err(IconError::Decode)?;
        image_node::mount(
            app,
            window,
            parent,
            resource.id,
            Size::new(self.size, self.size),
            self.color,
        )
        .map_err(IconError::Node)
    }
}

/// 图标挂载 / 反查失败（强类型；`Display` 给日志，`source` 给引擎错误链）。
#[derive(Debug)]
pub enum IconError {
    /// 目标窗口已关闭。
    UnknownWindow {
        /// 传入的窗口句柄。
        window: WindowId,
    },
    /// 逻辑尺寸非法（非有限 / `<= 0`）。
    InvalidSize {
        /// 传入的尺寸。
        size: f32,
    },
    /// 按名字反查不到变体。
    UnknownKind {
        /// 未知的名字。
        name: SmolStr,
    },
    /// 图标 PNG 解码 / 图集装箱失败（引擎错误）。
    Decode(EngineError),
    /// 建树失败（引擎错误）。
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
            IconError::Decode(error) => write!(formatter, "icon raster decode failed: {error}"),
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
    use image::{GenericImageView, ImageFormat, imageops::FilterType};
    use ui_core::math::Rect;

    use super::{DEFAULT_ICON_SIZE, IconError, IconKind, IconSpec, RasterTier, raster};
    use crate::kit::test_support::{image_primitives, test_app, test_app_with_scale};

    // 构建期清单（`build.rs` 写进 `OUT_DIR/icons/built.rs`）：资产 ⇄ 枚举一一对应的机器证据。
    include!(concat!(env!("OUT_DIR"), "/icons/built.rs"));

    /// 全部变体都登记在 [`IconKind::ALL`] 里，名字唯一，且与构建期产物 / 源 SVG 一一对应。
    ///
    /// 变异防线：删掉 `ALL` 的一项 / 改名 / 加一个没登记的 SVG ⇒ 本用例必须变红。
    #[test]
    fn every_icon_variant_has_a_unique_name_and_a_built_raster() {
        assert_eq!(
            IconKind::ALL.len(),
            IconKind::COUNT,
            "ALL 必须覆盖全部 55 个变体（少一项意味着某个图标在界面里画不出来）"
        );
        let mut names: Vec<&'static str> = IconKind::ALL.iter().map(|kind| kind.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), IconKind::COUNT, "图标名必须唯一");

        // 构建期清单与枚举是同一个集合（数量相等 + 双向包含）。
        assert_eq!(
            BUILT_ICON_FILES.len(),
            IconKind::COUNT,
            "构建期生成的 PNG 数必须等于枚举数：{BUILT_ICON_FILES:?}"
        );
        for kind in IconKind::ALL {
            assert!(
                BUILT_ICON_FILES.contains(&kind.name()),
                "{kind:?}（{}）没有构建期产物",
                kind.name()
            );
            // 旧模块的嵌入资产里必须有同名 SVG —— 构建期产物就是从它来的。
            let svg_path = format!("icons/phosphoricons/{}.svg", kind.name());
            assert!(
                crate::assets::Assets::get(&svg_path).is_some(),
                "{kind:?} 的源 SVG `{svg_path}` 不在嵌入资产里（映射写错了名字？）"
            );
        }
        // 名字 ⇄ 变体是双向可逆的（旧 `path()` 的字符串语义）。
        for kind in IconKind::ALL {
            assert_eq!(
                IconKind::from_name(kind.name()).expect("名字必须能反查"),
                kind
            );
        }
    }

    /// 两档 PNG 都能解码成 `RGBA8`、尺寸正好 48 / 96、字形是「白色 + alpha 覆盖」。
    ///
    /// 白色字形是运行期颜色调制的前提：任何非白像素都会被 `texel * modulation` 压暗，
    /// 主题色就不准了（变异「不替换成白色」⇒ 本用例变红）。
    ///
    /// **透明像素也必须是白色**（white matte）：引擎在 straight-alpha 空间先缩放、后预乘
    /// （`ui_core/src/resources/image.rs`），透明像素的 RGB 会插值进边缘像素。这里对**所有**
    /// 像素断言 RGB=255（含 `alpha == 0`）；变异「把 white matte 改回 RGB=0」⇒ 本用例与
    /// [`downscaled_icon_edges_have_no_dark_fringe`] 一起变红。
    #[test]
    fn every_icon_raster_is_a_white_glyph_png_of_the_expected_tier() {
        for kind in IconKind::ALL {
            for tier in RasterTier::ALL {
                let bytes = raster(kind, tier);
                assert!(
                    matches!(image::guess_format(bytes), Ok(ImageFormat::Png)),
                    "{kind:?}/{tier:?} 的产物不是 PNG"
                );
                let decoded = image::load_from_memory(bytes)
                    .unwrap_or_else(|error| panic!("{kind:?}/{tier:?} 的 PNG 无法解码：{error}"));
                let (width, height) = decoded.dimensions();
                assert_eq!(
                    (width, height),
                    (tier.pixels(), tier.pixels()),
                    "{kind:?}/{tier:?} 的像素尺寸不符"
                );
                let rgba = decoded.to_rgba8();
                let mut opaque = 0usize;
                for pixel in rgba.pixels() {
                    let [red, green, blue, alpha] = pixel.0;
                    assert_eq!(
                        (red, green, blue),
                        (0xFF, 0xFF, 0xFF),
                        "{kind:?}/{tier:?} 的像素 {pixel:?} 不是 white matte（alpha={alpha}）：\
                         RGB 必须全白，否则引擎缩放会把透明区的暗色插值进边缘"
                    );
                    if alpha > 0 {
                        opaque += 1;
                    }
                }
                assert!(opaque > 0, "{kind:?}/{tier:?} 整张图都是透明的（空字形）");
            }
        }
    }

    /// 复刻引擎的缩放管线（`ui_core/src/resources/image.rs` 的固定顺序），返回直通 alpha 的
    /// `RgbaImage`：这里只做「解码 → 缩放」两步 —— 预乘语义由 [`engine_premultiplied`] 单独算
    /// （引擎在缩放**之后**才预乘，正是本用例要证明的关键顺序）。
    fn engine_downscale(bytes: &[u8], side: u32) -> image::RgbaImage {
        let decoded = image::load_from_memory(bytes).expect("图标 PNG 可解码");
        assert_eq!(
            decoded.color(),
            image::ColorType::Rgba8,
            "图标产物必须是直通 alpha 的 RGBA8"
        );
        // 引擎的目标尺寸路径用 `FilterType::CatmullRom`（`resize_decoded` 的 D-10 分支），
        // 且 `image::imageops::resize` 不做 alpha 预乘（其文档：assumes alpha pre-multiplication）。
        image::imageops::resize(&decoded.to_rgba8(), side, side, FilterType::CatmullRom)
    }

    /// sRGB 解码（IEC 61966-2-1 分段函数；与引擎 `image.rs` 的 `srgb_decode` 同式）。
    fn srgb_decode(value: u8) -> f32 {
        let encoded = f32::from(value) / 255.0;
        if encoded <= 0.040_45 {
            encoded / 12.92
        } else {
            ((encoded + 0.055) / 1.055).powf(2.4)
        }
    }

    /// sRGB 编码（`srgb_decode` 的逆，含四舍五入；与引擎同式，含 `+0.5` 截断）。
    fn srgb_encode(linear: f32) -> u8 {
        let value = linear.clamp(0.0, 1.0);
        let encoded = if value <= 0.003_130_8 {
            value * 12.92
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        };
        (encoded * 255.0 + 0.5) as u8
    }

    /// 引擎上传给 GPU 的存储值（linear 预乘后 sRGB 编码；`image.rs` 的 `premultiply_channel`）。
    fn engine_premultiplied(color: u8, alpha: u8) -> u8 {
        srgb_encode(srgb_decode(color) * (f32::from(alpha) / 255.0))
    }

    /// **F1 像素级回归**：代表性图标下采样到实际绘制尺寸后，边缘像素不得变暗。
    ///
    /// 复刻引擎管线（`insert_owned_at_size` 的 48→16 / 96→32 目标变体）：解码 straight-alpha
    /// PNG → `CatmullRom` 缩放 → 线性预乘 → sRGB 重编码。白色字形在预乘空间的期望存储值是
    /// `srgb_encode(srgb_decode(255) × alpha)`；`alpha` 来自字形覆盖率。若透明像素的 RGB 是 0
    /// （旧缺陷），缩放会把覆盖率的边缘拉向黑色：实测 `check-circle-48 → 16` 最坏暗边
    /// **96/255**（38%）。white matte ⇒ RGB 通道是常量 255 ⇒ 缩放后仍恒 255 ⇒ 暗边恒为 0。
    ///
    /// 与旧 GPUI 渲染的差异（**已登记**，`spec-deltas/kit-assets.md` §11）：旧 `svg()` 由
    /// GPUI 的 `SvgRenderer` 在 paint 期用 resvg/tiny-skia 按元素尺寸的 2× 光栅化（矢量 AA，
    /// 单一缩放级）；新路径是构建期 resvg 光栅化（48/96）再经引擎 `CatmullRom` 下采样（两级
    /// 滤波）。差异只剩 AA 细节（覆盖率被近似两次，边缘可能略微更软），不存在颜色偏置；
    /// 本用例锁定的正是「不存在颜色偏置」这一可机器验证的部分。
    #[test]
    fn downscaled_icon_edges_have_no_dark_fringe() {
        // 含曲线 / 斜边 / 细笔画（最容易被插值压暗的形状）的代表性字形。
        let representatives = [
            IconKind::CheckCircle,
            IconKind::CircleNotch,
            IconKind::CaretDown,
            IconKind::X,
            IconKind::MagnifyingGlass,
            IconKind::GearSix,
        ];
        // 逻辑尺寸 16 px 是旧用法里最常见（`UnifiedSize::icon_size` 的 Large 档）的目标：
        // 1× 窗口 48→16、2× 窗口 96→32，都是 3× 下采样。
        for tier_side in [48u32, 96] {
            let target = tier_side / 3;
            for kind in representatives {
                let bytes = raster(
                    kind,
                    if tier_side == 48 {
                        RasterTier::Small48
                    } else {
                        RasterTier::Large96
                    },
                );
                let resized = engine_downscale(bytes, target);

                let mut fringe_pixels = 0usize;
                let mut worst_darkening = 0u8;
                for pixel in resized.pixels() {
                    let [red, green, blue, alpha] = pixel.0;
                    if alpha == 0 {
                        continue;
                    }
                    if alpha < 255 {
                        fringe_pixels += 1;
                        // 白色字形下采样后 RGB 必须仍是白色（white matte 的直接推论）。
                        let darkest = red.min(green).min(blue);
                        worst_darkening = worst_darkening.max(255 - darkest);
                    }
                    // 端到端：存储值（预乘 + sRGB 编码）必须等于「白色 × 覆盖率」。
                    let expected = srgb_encode(f32::from(alpha) / 255.0);
                    for component in [red, green, blue] {
                        let stored = engine_premultiplied(component, alpha);
                        assert!(
                            expected.abs_diff(stored) <= 1,
                            "{kind:?}/{tier_side}→{target} 的边缘像素 {pixel:?} 存储值 {stored} \
                             偏离期望 {expected} > 1：图标会带暗边"
                        );
                    }
                }
                assert!(
                    fringe_pixels > 0,
                    "{kind:?}/{tier_side}→{target} 没有部分覆盖像素（用例没有真的踩到边缘）"
                );
                assert_eq!(
                    worst_darkening, 0,
                    "{kind:?}/{tier_side}→{target} 的边缘像素最暗被压了 {worst_darkening}/255：\
                     透明像素的 RGB 不是 white matte"
                );
            }
        }
    }

    /// 档位选择：1× 用 48、高 DPI 用 96；异常 scale 按 1× 兜底。
    #[test]
    fn the_raster_tier_follows_the_window_scale() {
        assert_eq!(RasterTier::for_scale(1.0), RasterTier::Small48);
        assert_eq!(RasterTier::for_scale(0.75), RasterTier::Small48);
        assert_eq!(RasterTier::for_scale(1.25), RasterTier::Large96);
        assert_eq!(RasterTier::for_scale(1.5), RasterTier::Large96);
        assert_eq!(RasterTier::for_scale(2.0), RasterTier::Large96);
        assert_eq!(RasterTier::for_scale(3.0), RasterTier::Large96);
        assert_eq!(RasterTier::for_scale(f32::NAN), RasterTier::Small48);
        assert_eq!(RasterTier::for_scale(0.0), RasterTier::Small48);
        assert_eq!(RasterTier::Small48.pixels(), 48);
        assert_eq!(RasterTier::Large96.pixels(), 96);
    }

    /// 挂载后的节点是 16×16 的图片节点，paint 出 `Primitive::Image`，调制色就是 `IconSpec.color`。
    ///
    /// 变异防线：颜色调制失效（例如固定 `Color::WHITE`）⇒ 本用例变红。
    #[test]
    fn an_icon_mounts_one_image_primitive_with_the_modulation_color() {
        let mut app = test_app_with_scale(1.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let color = ui_core::paint::color::Color::from_rgba8([0x33, 0x99, 0xFF, 0xFF]);

        let node = IconSpec::new(IconKind::CheckCircle)
            .with_size(16.0)
            .with_color(color)
            .build(app.app_mut(), window, root)
            .expect("图标挂载");
        let _ = app.drain().expect("首帧");

        let bounds = app.probe().bounds(node).expect("图标节点几何");
        assert_eq!(
            bounds,
            Rect::new(0.0, 0.0, 16.0, 16.0),
            "图片节点的布局盒必须正好是逻辑尺寸（空文本 + line_height 1.0 的契约）"
        );
        let images = image_primitives(&app);
        assert_eq!(
            images.len(),
            1,
            "一个图标只应产生一个 Image 图元：{images:?}"
        );
        assert_eq!(images[0].modulation, color, "调制色必须是 IconSpec.color");
        assert_eq!(
            images[0].bounds,
            Rect::new(0.0, 0.0, 16.0, 16.0),
            "图片图元必须落在节点盒里"
        );
    }

    /// 2× 窗口取 96 档：图集里保留的**源字节**比 1× 窗口多出的正好是两档 PNG 的长度差。
    ///
    /// 这是「档位选择真的接进了挂载路径」的端到端证据（只测 `RasterTier::for_scale` 抓不到
    /// 「调用点写死 48」的变异）。每张图只插一次，`ResourceStats::used_bytes` 的差值因此只可能
    /// 来自源字节（页缓冲 / 图集 tile 在两档下相同）。
    #[test]
    fn a_2x_window_pays_for_the_96_pixel_raster() {
        let used_bytes = |scale: f32| {
            let mut app = test_app_with_scale(scale);
            let window = app.window_id();
            let root = app.app().window(window).expect("test window").tree().root();
            let _ = IconSpec::new(IconKind::GearSix)
                .with_size(16.0)
                .build(app.app_mut(), window, root)
                .expect("图标挂载");
            app.app().resources().stats().used_bytes
        };

        let one_x = used_bytes(1.0);
        let two_x = used_bytes(2.0);
        let expected = (raster(IconKind::GearSix, RasterTier::Large96).len()
            - raster(IconKind::GearSix, RasterTier::Small48).len()) as u64;
        assert!(
            expected > 0,
            "两档 PNG 必须有不同的字节长度（否则本用例证明不了档位）"
        );
        assert_eq!(
            two_x.checked_sub(one_x),
            Some(expected),
            "2× 窗口必须保留 96 档的源字节（1× = {one_x} B，2× = {two_x} B，期望差 {expected} B）"
        );
    }

    /// 未知名字 / 非法尺寸都是强类型错误（旧语义里「错误字符串」没有位置）。
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
            let error = IconSpec::new(IconKind::X)
                .with_size(size)
                .build(app.app_mut(), window, root)
                .expect_err("非法尺寸必须失败");
            assert!(
                matches!(&error, IconError::InvalidSize { size: actual } if actual.to_bits() == size.to_bits()),
                "size={size} 的错误分类不对：{error:?}"
            );
        }
        assert_eq!(DEFAULT_ICON_SIZE, 16.0, "默认尺寸是旧用法里的常见值");
    }

    /// 已关闭的窗口是强类型错误（不是 panic、也不是静默落在别处）。
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

    /// 图标节点是**装饰性叶子**：a11y 树里必须 `hidden`，占位空格不得被 AT 播报。
    ///
    /// 引擎没有图片节点，图标走「空格文本 + 行内盒图片」（`kit::image_node`）——占位空格是
    /// 实现细节，对 a11y 必须不可见。审查的变异「删掉 `.a11y(hidden)`」曾存活（38/38 全绿），
    /// 本用例锁死它：去掉那三行后 `record.hidden` 变 false / 可见节点里出现空白文本 ⇒ 变红。
    #[test]
    fn icon_nodes_are_hidden_from_the_a11y_tree() {
        let mut app = test_app();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let node = IconSpec::new(IconKind::CheckCircle)
            .with_size(16.0)
            .build(app.app_mut(), window, root)
            .expect("图标挂载");
        let _ = app.drain().expect("首帧");

        let snapshot = app.a11y().expect("a11y 快照（开启捕获后的首帧是全树）");
        if let Some(record) = snapshot.node_for(node) {
            assert!(
                record.hidden,
                "图标节点必须在 a11y 树里标记 hidden（占位空格是装饰性文本）：{record:?}"
            );
        }
        // 无论引擎把 hidden 节点留在快照里还是整棵剔除，「可见节点」视图都不得含空白文本。
        for record in snapshot.visible_nodes_in_batch() {
            if let Some(label) = record.label_text() {
                assert!(
                    !label.trim().is_empty(),
                    "可见的 a11y 节点不得只有空白文本（图标占位空格被播报了）：{record:?}"
                );
            }
        }
        assert!(
            snapshot
                .nodes()
                .iter()
                .all(|record| record.engine != node || record.hidden),
            "图标节点只能以 hidden 的形式出现在 a11y 树里"
        );
    }
}

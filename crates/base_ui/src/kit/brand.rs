//! 品牌 logo：7 个内置品牌的路径 / 字节 / 固有尺寸，以及图片节点挂载。
//!
//! # 与旧实现的关系
//!
//! 旧 `crate::brand` 是「7 条路径常量 + 一个 `Brand` 元素（`img(path)`）」：路径常量给
//! **命名者**（Widget 占位文案里的内联 `<img>`）用，`Brand` 给**画图者**用。两条用途在这里
//! 合成一份数据：`BrandKind` 是唯一事实源（路径 / 字节 / 固有尺寸都从它取），`Brand` 负责把
//! logo 挂成图片节点。
//!
//! # 显式尺寸是 **Contain**（等比内嵌 + 居中），不是拉伸
//!
//! 旧 `Brand` 的 `img()` 用 GPUI `Img` 的默认 `ImageStyle`，其 `object_fit` 是
//! **`ObjectFit::Contain`**（`gpui-pre-0.3.12/src/elements/img.rs` 的 `impl Default`；paint 时
//! `object_fit.get_bounds(bounds, image_size)`，`src/style.rs`）。旧调用点给的是正方形盒子
//! （`profile_ui/src/menu.rs` 的 `.size(px(14.))`、`base_ui` 的 `.size_4()`），于是非正方形
//! logo（Ethereum 44×72、Solana 81×72）是**等比内缩 + 居中**、两侧留白，而不是被拉扁。
//! 新实现按同一语义复刻：[`Brand::build`] 用 [`contain`] 算出图元矩形，留白时先挂一个
//! `display_size` 大小的居中容器再挂图片节点（引擎图元没有 object-fit，只能由布局承担）。
//!
//! 字节来自 [`crate::kit::assets`] 的注册表（`include_bytes!`）；运行期用引擎
//! `ResourceCache::insert_owned_at_size` 取变体（同内容 + 同变体只占一个 tile）。品牌 logo 是
//! **彩色** PNG，调制色恒为 `Color::WHITE`（不染色）；白字调制的图片语义见
//! [`crate::kit::icon`] 的模块文档。
//!
//! 合规口径沿用旧注释：`Longbridge` 的 PNG 是占位图，正式资产要在取得展示权后替换；路径不变
//! （设计 §8.5 的合规缺口只登记，不在本批范围）。

use std::fmt;

use taffy::style::{AlignItems, Dimension, JustifyContent};
use ui_core::{
    app::App,
    error::EngineError,
    math::{Rect, Size},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{color::Color, style::Style},
    window::WindowId,
};

use crate::kit::{assets, image_node};

/// 一个内置品牌（= 旧 `crate::brand` 的 7 条路径常量）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum BrandKind {
    Binance,
    Ethereum,
    Hyperliquid,
    Longbridge,
    Okx,
    Polymarket,
    Solana,
}

impl BrandKind {
    /// 全部品牌（顺序 = 旧模块的常量顺序：Ethereum / Solana / Binance / OKX / Hyperliquid /
    /// Polymarket / Longbridge 的字母序，便于清单比对）。
    pub const ALL: [BrandKind; 7] = [
        BrandKind::Binance,
        BrandKind::Ethereum,
        BrandKind::Hyperliquid,
        BrandKind::Longbridge,
        BrandKind::Okx,
        BrandKind::Polymarket,
        BrandKind::Solana,
    ];

    /// 品牌名（稳定标识；配置 / 日志用）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            BrandKind::Binance => "Binance",
            BrandKind::Ethereum => "Ethereum",
            BrandKind::Hyperliquid => "Hyperliquid",
            BrandKind::Longbridge => "Longbridge",
            BrandKind::Okx => "OKX",
            BrandKind::Polymarket => "Polymarket",
            BrandKind::Solana => "Solana",
        }
    }

    /// 资产路径（相对 `crates/base_ui/assets`；与旧 `crate::brand::{ETHEREUM, ..}` 常量逐字一致）。
    #[must_use]
    pub const fn asset_path(self) -> &'static str {
        match self {
            BrandKind::Binance => "brands/Binance/binance.png",
            BrandKind::Ethereum => "brands/Ethereum/colored@72px.png",
            BrandKind::Hyperliquid => "brands/Hyperliquid/hyperliquid.png",
            BrandKind::Longbridge => "brands/Longbridge/longbridge.png",
            BrandKind::Okx => "brands/OKX/okx.png",
            BrandKind::Polymarket => "brands/Polymarket/polymarket.png",
            BrandKind::Solana => "brands/Solana/colored@72px.png",
        }
    }

    /// PNG 字节（注册表里必有；`assets::ASSETS` 与 [`BrandKind::ALL`] 的对应由测试锁定）。
    #[must_use]
    pub fn bytes(self) -> &'static [u8] {
        assets::bytes(self.asset_path()).expect("品牌 PNG 必须登记在 kit::assets::ASSETS 里")
    }

    /// 固有尺寸（逻辑像素；文件头读取，不解码像素）。Ethereum / Solana 是 44×72 / 81×72 的
    /// 非正方形图，长宽比必须保真（旧 `img()` 的固有尺寸语义）。
    #[must_use]
    pub fn native_size(self) -> Size {
        assets::find(self.asset_path())
            .and_then(assets::Asset::native_size)
            .expect("品牌 PNG 必须是可读文件头的位图（PNG）")
    }
}

/// 一个品牌 logo 的绘制声明（尺寸可选：不给 = 固有尺寸）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Brand {
    /// 画哪个品牌。
    pub kind: BrandKind,
    /// 逻辑边长（正方形**容器盒**；`None` = 用固有宽高）。
    ///
    /// 语义是旧 GPUI `img()` 的默认 `ObjectFit::Contain`（`ImageStyle::default()`）：logo
    /// **等比内嵌**进这个正方形并居中，非正方形 logo（Ethereum 44×72 / Solana 81×72）会留白，
    /// 而不是被拉伸成正方形（见模块文档与 spec-delta §6 第 5 条）。
    pub size: Option<f32>,
}

impl Brand {
    /// 固有尺寸的 logo。
    #[must_use]
    pub const fn new(kind: BrandKind) -> Self {
        Self { kind, size: None }
    }

    /// 正方形容器盒的边长（逻辑像素）。
    #[must_use]
    pub const fn with_size(mut self, side: f32) -> Self {
        self.size = Some(side);
        self
    }

    /// 容器盒尺寸（逻辑像素；正方形盒 / 固有尺寸）。
    ///
    /// 图元本身按 Contain 等比内嵌（[`contain`]），所以非正方形 logo 在正方形盒里的图元尺寸
    /// **小于**盒尺寸；唯一没有留白的情况是盒子与 logo 同宽高比（正方形品牌、或不给尺寸）。
    #[must_use]
    pub fn display_size(self) -> Size {
        match self.size {
            Some(side) => Size::new(side, side),
            None => self.kind.native_size(),
        }
    }

    /// 把 logo 挂到 `parent` 末尾，返回**布局容器**句柄（无留白时就是图片节点本身）。
    ///
    /// 与 [`crate::kit::icon::IconSpec::build`] 同一条图片通道（行内盒图片），差别是：
    /// - 资源是**彩色**品牌 PNG、调制色恒为白色；
    /// - 显式尺寸按旧 GPUI `ObjectFit::Contain` 语义**等比内嵌 + 居中**：无留白（正方形品牌 /
    ///   固有尺寸）直接挂图片节点；有留白时先挂一个 `display_size` 大小的 flex 容器
    ///   （`justify_content` / `align_items` = CENTER），图片节点是它唯一的子节点。返回的句柄
    ///   布局盒恒为 [`Brand::display_size`]。
    ///
    /// # Errors
    /// - [`BrandError::UnknownWindow`]：窗口已关闭；
    /// - [`BrandError::InvalidSize`]：显式尺寸非有限或 `<= 0`；
    /// - [`BrandError::Decode`]：PNG 解码 / 图集装箱失败（引擎强类型 `EngineError`）；
    /// - [`BrandError::Node`]：父节点陈旧 / 布局叶子等建树错误（引擎强类型 `EngineError`）。
    pub fn build(
        self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, BrandError> {
        if let Some(side) = self.size
            && (!side.is_finite() || side <= 0.0)
        {
            return Err(BrandError::InvalidSize { size: side });
        }
        let scale = app
            .window(window)
            .ok_or(BrandError::UnknownWindow { window })?
            .scale()
            .factor();
        let frame = self.display_size();
        let image = contain(self.kind.native_size(), frame);
        let draw = image.size;
        // 物理目标按**图元**尺寸取（不是容器盒）：留白区不该占图集分辨率。
        let physical = Size::new(draw.width * scale, draw.height * scale);
        let resource = app
            .resources_mut()
            .insert_owned_at_size(window, self.kind.bytes(), physical)
            .map_err(BrandError::Decode)?;
        if image.origin.x == 0.0 && image.origin.y == 0.0 {
            return image_node::mount(app, window, parent, resource.id, draw, Color::WHITE)
                .map_err(BrandError::Node);
        }
        let mut style = Style::default();
        style.inner.size.width = Dimension::length(frame.width);
        style.inner.size.height = Dimension::length(frame.height);
        style.inner.justify_content = JustifyContent::CENTER;
        style.inner.align_items = AlignItems::CENTER;
        let container = NodeBuilder::boxed()
            .style(style)
            .build(
                app.window_mut(window)
                    .ok_or(BrandError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )
            .map_err(BrandError::Node)?;
        image_node::mount(app, window, container, resource.id, draw, Color::WHITE)
            .map_err(BrandError::Node)?;
        Ok(container)
    }
}

/// 等比内缩（CSS `object-fit: contain`）：把 `native` 等比缩放进 `frame` 并居中。
///
/// 公式与旧 GPUI `ObjectFit::Contain::get_bounds`（`gpui-pre-0.3.12/src/style.rs`）逐式相同：
/// 按「容器宽高比 vs 图宽高比」选约束方向，再把余量**均分**到两侧。返回**相对 frame 左上角**
/// 的图元矩形（逻辑像素）—— `x` / `y` 就是留白。
///
/// 例：Ethereum（44×72）在 36×36 的盒里 ⇒ 22×36，左右各留白 7；Solana（81×72）⇒ 36×32，
/// 上下各留白 2。Contain 允许**放大**（10×10 放进 40×40 ⇒ 40×40），与 GPUI 一致（不是 ScaleDown）。
fn contain(native: Size, frame: Size) -> Rect {
    let native_ratio = native.width / native.height;
    let frame_ratio = frame.width / frame.height;
    let draw = if frame_ratio > native_ratio {
        Size::new(native.width * (frame.height / native.height), frame.height)
    } else {
        Size::new(frame.width, native.height * (frame.width / native.width))
    };
    Rect::new(
        (frame.width - draw.width) / 2.0,
        (frame.height - draw.height) / 2.0,
        draw.width,
        draw.height,
    )
}

/// 品牌 logo 挂载失败（强类型；`Display` 给日志，`source` 给引擎错误链）。
#[derive(Debug)]
pub enum BrandError {
    /// 目标窗口已关闭。
    UnknownWindow {
        /// 传入的窗口句柄。
        window: WindowId,
    },
    /// 显式尺寸非法（非有限 / `<= 0`）。
    InvalidSize {
        /// 传入的边长。
        size: f32,
    },
    /// 品牌 PNG 解码 / 图集装箱失败（引擎错误）。
    Decode(EngineError),
    /// 建树失败（引擎错误）。
    Node(EngineError),
}

impl fmt::Display for BrandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrandError::UnknownWindow { window } => {
                write!(formatter, "brand target window {window:?} is closed")
            }
            BrandError::InvalidSize { size } => {
                write!(formatter, "brand logo size {size} must be finite and > 0")
            }
            BrandError::Decode(error) => write!(formatter, "brand logo decode failed: {error}"),
            BrandError::Node(error) => write!(formatter, "brand logo node mount failed: {error}"),
        }
    }
}

impl std::error::Error for BrandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BrandError::Decode(error) | BrandError::Node(error) => Some(error),
            BrandError::UnknownWindow { .. } | BrandError::InvalidSize { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{
        math::{Rect, Size},
        paint::{color::Color, primitive::Image},
    };

    use super::{Brand, BrandError, BrandKind, contain};
    use crate::kit::test_support::{assert_single_image, bounds, image_primitives, test_app};

    /// 7 个品牌：名字唯一、路径与旧模块的常量逐字一致、字节与旧 rust-embed 资产逐字节一致。
    #[test]
    fn every_brand_matches_the_legacy_paths_and_bytes() {
        assert_eq!(BrandKind::ALL.len(), 7);
        let legacy = [
            (BrandKind::Ethereum, crate::brand::ETHEREUM),
            (BrandKind::Solana, crate::brand::SOLANA),
            (BrandKind::Binance, crate::brand::BINANCE),
            (BrandKind::Okx, crate::brand::OKX),
            (BrandKind::Hyperliquid, crate::brand::HYPERLIQUID),
            (BrandKind::Polymarket, crate::brand::POLYMARKET),
            (BrandKind::Longbridge, crate::brand::LONGBRIDGE),
        ];
        for (kind, path) in legacy {
            assert_eq!(kind.asset_path(), path, "{kind:?} 的路径与旧常量不一致");
            let old = crate::assets::Assets::get(path).expect("旧嵌入资产里必须有同名文件");
            assert_eq!(kind.bytes(), old.data.as_ref(), "{kind:?} 的字节漂移");
        }
        let mut names: Vec<&str> = BrandKind::ALL.iter().map(|kind| kind.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 7, "品牌名必须唯一");
    }

    /// 固有尺寸保真（Ethereum / Solana 是非正方形）；显式尺寸是**正方形盒**（图元在其内等比内嵌）。
    #[test]
    fn native_sizes_keep_the_aspect_ratio_and_explicit_sizes_are_square_boxes() {
        assert_eq!(
            BrandKind::Ethereum.native_size(),
            ui_core::math::Size::new(44.0, 72.0)
        );
        assert_eq!(
            BrandKind::Solana.native_size(),
            ui_core::math::Size::new(81.0, 72.0)
        );
        assert_eq!(
            Brand::new(BrandKind::Ethereum).display_size(),
            ui_core::math::Size::new(44.0, 72.0)
        );
        assert_eq!(
            Brand::new(BrandKind::Ethereum)
                .with_size(24.0)
                .display_size(),
            ui_core::math::Size::new(24.0, 24.0)
        );
    }

    /// 容器盒 / 图元盒的浮点容差断言（`contain` 的除法在 f32 末位有抖动）。
    fn assert_rect_near(actual: Rect, expected: Rect) {
        let eps = 1e-2;
        assert!(
            (actual.origin.x - expected.origin.x).abs() < eps
                && (actual.origin.y - expected.origin.y).abs() < eps
                && (actual.size.width - expected.size.width).abs() < eps
                && (actual.size.height - expected.size.height).abs() < eps,
            "期望 {expected:?}，实际 {actual:?}"
        );
    }

    /// 挂一个品牌并返回（容器盒, 唯一图元）。每个 `TestApp` 只挂一个品牌。
    fn measured(brand: Brand) -> (Rect, Image) {
        let mut app = test_app();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let node = brand.build(app.app_mut(), window, root).expect("品牌挂载");
        let _ = app.drain().expect("首帧");
        let frame = bounds(&app, node).expect("容器布局盒");
        (frame, assert_single_image(&app))
    }

    /// **F2 纯几何**：`contain` 在 1:1 / 2:1 / 1:2 与不同比例容器下的内缩 + 居中。
    ///
    /// 公式与旧 GPUI `ObjectFit::Contain::get_bounds` 逐式相同；这里锁定「等比、居中、留白」
    /// 三条可机器验证的性质（把 `build` 改回拉伸填充 ⇒ 集成用例变红，见
    /// `non_square_brands_are_contained_in_the_requested_square`）。
    #[test]
    fn contain_keeps_the_aspect_ratio_and_centers_the_letterbox() {
        // 1:1 图放进 2:1 容器：上下贴满、左右各留白 50。
        assert_rect_near(
            contain(Size::new(100.0, 100.0), Size::new(200.0, 100.0)),
            Rect::new(50.0, 0.0, 100.0, 100.0),
        );
        // 2:1 图放进 1:1 容器：左右贴满、上下各留白 25。
        assert_rect_near(
            contain(Size::new(200.0, 100.0), Size::new(100.0, 100.0)),
            Rect::new(0.0, 25.0, 100.0, 50.0),
        );
        // 1:2 图放进 1:1 容器：上下贴满、左右各留白 25。
        assert_rect_near(
            contain(Size::new(100.0, 200.0), Size::new(100.0, 100.0)),
            Rect::new(25.0, 0.0, 50.0, 100.0),
        );
        // 1:1 图放进 1:1 容器：没有留白（盒子与图元重合）。
        assert_rect_near(
            contain(Size::new(100.0, 100.0), Size::new(100.0, 100.0)),
            Rect::new(0.0, 0.0, 100.0, 100.0),
        );
        // 4:3 图放进 3:4 容器：宽度贴满、上下各留白 87.5。
        assert_rect_near(
            contain(Size::new(400.0, 300.0), Size::new(300.0, 400.0)),
            Rect::new(0.0, 87.5, 300.0, 225.0),
        );
        // Contain 允许放大（不是 ScaleDown）：10×10 放进 40×40 ⇒ 40×40。
        assert_rect_near(
            contain(Size::new(10.0, 10.0), Size::new(40.0, 40.0)),
            Rect::new(0.0, 0.0, 40.0, 40.0),
        );

        // 性质断言（不依赖具体数字）：图元宽高比 == 源宽高比、图元不溢出、x/y 余量两侧相等。
        for (native, frame) in [
            (Size::new(44.0, 72.0), Size::new(36.0, 36.0)),
            (Size::new(81.0, 72.0), Size::new(36.0, 36.0)),
            (Size::new(300.0, 100.0), Size::new(64.0, 48.0)),
            (Size::new(100.0, 300.0), Size::new(64.0, 48.0)),
        ] {
            let image = contain(native, frame);
            let native_ratio = native.width / native.height;
            let drawn_ratio = image.size.width / image.size.height;
            assert!(
                (native_ratio - drawn_ratio).abs() < 1e-4,
                "{native:?} → {frame:?} 的图元宽高比漂移：{drawn_ratio} vs {native_ratio}"
            );
            assert!(
                image.size.width <= frame.width + 1e-3 && image.size.height <= frame.height + 1e-3,
                "{native:?} → {frame:?} 的图元溢出容器：{image:?}"
            );
            assert!(
                (image.origin.x - (frame.width - image.size.width) / 2.0).abs() < 1e-4
                    && (image.origin.y - (frame.height - image.size.height) / 2.0).abs() < 1e-4,
                "{native:?} → {frame:?} 没有居中：{image:?}"
            );
        }
    }

    /// **F2 集成**：真实品牌在正方形盒里等比内嵌 + 居中 + 留白；正方形品牌/固有尺寸无留白。
    ///
    /// 变异「改回拉伸填充」（图元 = 容器盒）⇒ Ethereum 的 22×36 / Solana 的 36×32 断言变红。
    #[test]
    fn non_square_brands_are_contained_in_the_requested_square() {
        // Ethereum 44×72 放进 36×36：等比内缩到 22×36，左右各留白 7。
        let (frame, image) = measured(Brand::new(BrandKind::Ethereum).with_size(36.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 36.0, 36.0));
        assert_rect_near(image.bounds, Rect::new(7.0, 0.0, 22.0, 36.0));
        assert_eq!(image.modulation, Color::WHITE, "品牌图不染色");

        // Solana 81×72 放进 36×36：等比内缩到 36×32，上下各留白 2。
        let (frame, image) = measured(Brand::new(BrandKind::Solana).with_size(36.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 36.0, 36.0));
        assert_rect_near(image.bounds, Rect::new(0.0, 2.0, 36.0, 32.0));

        // 正方形品牌：盒子与图元重合，没有留白（也不需要多余的容器节点）。
        let (frame, image) = measured(Brand::new(BrandKind::Binance).with_size(36.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 36.0, 36.0));
        assert_rect_near(image.bounds, Rect::new(0.0, 0.0, 36.0, 36.0));

        // 固有尺寸（不给尺寸）：沿用 PNG 固有宽高，同样不拉伸。
        let (frame, image) = measured(Brand::new(BrandKind::Ethereum));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 44.0, 72.0));
        assert_rect_near(image.bounds, Rect::new(0.0, 0.0, 44.0, 72.0));
    }

    /// 挂载：一个 `Image` 图元、不染色（`Color::WHITE`）、容器盒 = 给定尺寸。
    #[test]
    fn a_brand_mounts_an_unmodulated_image_node() {
        let (frame, image) = measured(Brand::new(BrandKind::Solana).with_size(32.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 32.0, 32.0));
        assert_eq!(image.modulation, Color::WHITE, "品牌图不染色");
        // Solana 81×72 放进 32×32：等比内缩到 32×28.44，上下各留白 1.78。
        assert_rect_near(image.bounds, Rect::new(0.0, 1.778, 32.0, 28.444));
    }

    /// 非法尺寸（0 / 负数 / NaN / ±∞）/ 已关闭窗口是强类型错误。
    #[test]
    fn brand_errors_are_typed() {
        let mut app = test_app();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let error = Brand::new(BrandKind::Binance)
                .with_size(size)
                .build(app.app_mut(), window, root)
                .expect_err("非法尺寸必须失败");
            assert!(
                matches!(&error, BrandError::InvalidSize { size: actual } if actual.to_bits() == size.to_bits()),
                "size={size} 的错误分类不对：{error:?}"
            );
        }

        assert!(app.app_mut().close_window(window), "窗口必须存在");
        let error = Brand::new(BrandKind::Binance)
            .build(app.app_mut(), window, root)
            .expect_err("窗口已关闭");
        assert!(matches!(error, BrandError::UnknownWindow { window: closed } if closed == window));
    }

    /// 旧的 `image_primitives` 视图在品牌挂载后只有一张图（回归：容器 + 子节点不得复制图元）。
    #[test]
    fn a_letterboxed_brand_still_encodes_exactly_one_image_primitive() {
        let mut app = test_app();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        Brand::new(BrandKind::Ethereum)
            .with_size(36.0)
            .build(app.app_mut(), window, root)
            .expect("品牌挂载");
        let _ = app.drain().expect("首帧");
        assert_eq!(image_primitives(&app).len(), 1);
    }
}

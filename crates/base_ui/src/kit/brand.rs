//! 品牌 logo：7 个内置品牌的路径 / 字节 / 固有尺寸，以及 E7 图片节点挂载。
//!
//! # 与旧实现的关系
//!
//! 旧 `crate::brand` 是「7 条路径常量 + 一个 `Brand` 元素（`img(path)`）」：路径常量给
//! **命名者**（Widget 占位文案里的内联 `<img>`）用，`Brand` 给**画图者**用。两条用途在这里
//! 合成一份数据：`BrandKind` 是唯一事实源（路径 / 字节 / 固有尺寸都从它取），`Brand` 负责把
//! logo 挂成 E7 图片节点。
//!
//! # 显式尺寸是 **Contain**（等比内嵌 + 居中），不是拉伸
//!
//! 旧 `Brand` 的 `img()` 用 GPUI `Img` 的默认 `ImageStyle`，其 `object_fit` 是
//! **`ObjectFit::Contain`**（`gpui-pre-0.3.12/src/elements/img.rs` 的 `impl Default`；paint 时
//! `object_fit.get_bounds(bounds, image_size)`，`src/style.rs`）。旧调用点给的是正方形盒子
//! （`profile_ui/src/menu.rs` 的 `.size(px(14.))`、`base_ui` 的 `.size_4()`），于是非正方形
//! logo（Ethereum 44×72、Solana 81×72）是**等比内缩 + 居中**、两侧留白，而不是被拉扁。
//!
//! E7 图片节点的 [`ImageFit::Contain`] 就是同一条 CSS `object-fit: contain` 语义：节点盒是
//! 布局盒（显式尺寸 / 固有尺寸），引擎在 paint 期把 tile 等比内嵌 + 居中（图元 bounds 小于节点
//! 盒）。本模块因此**不再手算 contain 几何**：给节点显式 `Style` 尺寸 + `ImageFit::Contain`，
//! 几何由引擎负责（旧实现的私有 `contain()` 与容器节点已删除）。
//!
//! # 字节与调制
//!
//! 字节来自 [`crate::kit::assets`] 的注册表（`include_bytes!`）；运行期用引擎
//! `ResourceCache::insert_owned_at_size`（= `insert_for_display` 的窗口归属版）取变体（同内容 +
//! 同变体只占一个 tile）。品牌 logo 是**彩色** PNG，调制色恒为 `Color::WHITE`（不染色）。
//!
//! # DPI
//!
//! 与 [`crate::kit::icon`] 同一条语义：挂载时按 `Window::scale()` 取物理变体，DPI 变化后由
//! [`Brand::reselect`] 重取变体并 `Window::set_image_resource`（E7 只重解析内在尺寸，不换资产；
//! 节点的显式 `Style` 尺寸保证布局不跳）。
//!
//! # a11y
//!
//! 有 [`Brand::with_alt`] 时挂 E7 `Image` 角色 + `A11yOverride::label`（不隐藏）；没有 alt 时不写
//! a11y 声明，引擎兜底把它当无名的图片隐藏，并记 `MissingAltText { kind: Image }` —— 品牌 logo
//! 承载「哪家交易所」的语义，这条诊断就是「用 `with_alt` 补可访问名」的提示（显式装饰声明是
//! 图标侧的做法：`IconSpec` 没有 label 通道，因此固定显式 `hidden` 免诊断）。
//!
//! 合规口径沿用旧注释：`Longbridge` 的 PNG 是占位图，正式资产要在取得展示权后替换；路径不变
//! （设计 §8.5 的合规缺口只登记，不在本批范围）。

use std::fmt;

use smol_str::SmolStr;
use taffy::style::Dimension;
use ui_core::{
    a11y::A11yOverride,
    app::App,
    error::EngineError,
    math::Size,
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit},
    paint::{color::Color, style::Style},
    resources::ResourceImage,
    window::WindowId,
};

use crate::kit::assets;

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
#[derive(Clone, Debug, PartialEq)]
pub struct Brand {
    /// 画哪个品牌。
    pub kind: BrandKind,
    /// 逻辑边长（正方形**节点盒**；`None` = 用固有宽高）。
    ///
    /// 语义是旧 GPUI `img()` 的默认 `ObjectFit::Contain`：logo 由 E7 [`ImageFit::Contain`]
    /// **等比内嵌**进这个正方形并居中，非正方形 logo（Ethereum 44×72 / Solana 81×72）会留白，
    /// 而不是被拉伸成正方形（见模块文档与 spec-delta §6 第 5 条）。
    pub size: Option<f32>,
    /// 可访问名（alt）。`None` = 不提供可访问名：引擎兜底 hidden 并记
    /// `MissingAltText { kind: Image }`（有语义的品牌 logo 应给 alt）。
    pub alt: Option<SmolStr>,
}

impl Brand {
    /// 固有尺寸、无 alt 的 logo。
    #[must_use]
    pub const fn new(kind: BrandKind) -> Self {
        Self {
            kind,
            size: None,
            alt: None,
        }
    }

    /// 正方形节点盒的边长（逻辑像素）。
    #[must_use]
    pub const fn with_size(mut self, side: f32) -> Self {
        self.size = Some(side);
        self
    }

    /// 可访问名（旧 GPUI `img().alt(..)` 的对位；E7 走 `A11yOverride::label`）。
    #[must_use]
    pub fn with_alt(mut self, alt: impl Into<SmolStr>) -> Self {
        self.alt = Some(alt.into());
        self
    }

    /// 节点盒尺寸（逻辑像素；正方形盒 / 固有尺寸）。
    ///
    /// E7 `Contain` 的图元 bounds 可能小于节点盒（非正方形 logo 的留白）；唯一没有留白的情况是
    /// 盒子与 logo 同宽高比（正方形品牌、或不给尺寸）。
    #[must_use]
    pub fn display_size(&self) -> Size {
        match self.size {
            Some(side) => Size::new(side, side),
            None => self.kind.native_size(),
        }
    }

    /// 把 logo 挂到 `parent` 末尾，返回 E7 图片节点句柄（布局盒 = [`Brand::display_size`]）。
    ///
    /// 与 [`crate::kit::icon::IconSpec::build`] 同一条 E7 图片通道，差别是：
    /// - 资源是**彩色**品牌 PNG，调制色 `Color::WHITE`（不染色）；
    /// - [`ImageFit::Contain`] = 旧 `ObjectFit::Contain` 语义（等比内嵌 + 居中，图元 bounds 在
    ///   节点盒内留白）：几何由引擎在 paint 期算，本模块不再手算 contain、也不再挂容器节点；
    /// - 有 [`Brand::alt`] 时挂 `Image` 角色 + label；没有 alt 时引擎兜底 hidden，并记
    ///   `MissingAltText { kind: Image }`（提示用 [`Brand::with_alt`] 补可访问名）。
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
        let frame = self.display_size();
        let scale = app
            .window(window)
            .ok_or(BrandError::UnknownWindow { window })?
            .scale()
            .factor();
        let resource = self.resource_for(app, window, scale)?;

        let mut style = Style::default();
        style.inner.size.width = Dimension::length(frame.width);
        style.inner.size.height = Dimension::length(frame.height);
        let mut builder = NodeBuilder::image(resource.id, ImageFit::Contain)
            .image_modulation(Color::WHITE)
            .style(style);
        if let Some(alt) = self.alt {
            builder = builder.a11y(A11yOverride {
                label: Some(alt),
                ..A11yOverride::default()
            });
        }
        builder
            .build(
                app.window_mut(window)
                    .ok_or(BrandError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )
            .map_err(BrandError::Node)
    }

    /// DPI 变化后重选档位并把新变体换到已挂载的品牌图片节点上（E7 `set_image_resource`）。
    ///
    /// 语义与 [`crate::kit::icon::IconSpec::reselect`] 相同：节点盒是显式 `Style` 尺寸，换变体
    /// 只影响采样密度、不改变布局；帧外调用后由 `App::request_repaint` 保证下一帧重绘。
    ///
    /// # Errors
    /// - [`BrandError::UnknownWindow`] / [`BrandError::InvalidSize`]：同 [`Brand::build`]；
    /// - [`BrandError::NotImageNode`]：`node` 不是品牌图节点（例如传成了别的节点）；
    /// - [`BrandError::Decode`] / [`BrandError::Node`]：引擎强类型错误（陈旧节点是
    ///   `Node(EngineError::StaleNode { .. })`）。
    pub fn reselect(
        self,
        app: &mut App,
        window: WindowId,
        node: NodeId,
    ) -> Result<ResourceImage, BrandError> {
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
        let resource = self.resource_for(app, window, scale)?;
        let accepted = app
            .window_mut(window)
            .ok_or(BrandError::UnknownWindow { window })?
            .set_image_resource(node, resource.id)
            .map_err(BrandError::Node)?;
        if !accepted {
            return Err(BrandError::NotImageNode { node });
        }
        app.request_repaint(window).map_err(BrandError::Node)?;
        Ok(resource)
    }

    /// 按 `scale` 取图集变体（`build` / `reselect` 共用的插入路径）。
    ///
    /// 物理目标 = 节点盒 × scale：引擎按目标尺寸等比 fit 解码，所以非正方形 logo 的留白区不占
    /// 图集分辨率（Ethereum 44×72 在 36×36 盒里解出 22×36）。
    fn resource_for(
        &self,
        app: &mut App,
        window: WindowId,
        scale: f32,
    ) -> Result<ResourceImage, BrandError> {
        let frame = self.display_size();
        let physical = Size::new(frame.width * scale, frame.height * scale);
        app.resources_mut()
            .insert_owned_at_size(window, self.kind.bytes(), physical)
            .map_err(BrandError::Decode)
    }
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
    /// `reselect` 的目标节点不是品牌图片节点。
    NotImageNode {
        /// 传入的节点句柄。
        node: NodeId,
    },
    /// 品牌 PNG 解码 / 图集装箱失败（引擎错误）。
    Decode(EngineError),
    /// 建树 / 换句柄失败（引擎错误）。
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
            BrandError::NotImageNode { node } => {
                write!(
                    formatter,
                    "brand reselect target {node:?} is not an image node"
                )
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
            BrandError::UnknownWindow { .. }
            | BrandError::InvalidSize { .. }
            | BrandError::NotImageNode { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{
        a11y::{A11yDiagnostic, A11yDrawKind, A11yRole},
        math::{Rect, Scale, Size},
        node::{builder::NodeBuilder, id::NodeId, image::ImageFit, tree::NodeKind},
        paint::{color::Color, primitive::Image},
        test_support::app::TestApp,
    };

    use super::{Brand, BrandError, BrandKind};
    use crate::{
        kit::test_support::{
            assert_single_image, bounds, image_primitives, test_app, test_app_with_scale,
        },
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

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

    /// 浮点容差断言（E7 `Contain` 的等比缩放 / 居中是 f32 计算，末位有抖动）。
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

    /// 挂一个品牌并返回（节点盒, 唯一图元）；同时锁定节点形态（E7 图片叶子 + `Contain`）。
    fn measured(brand: Brand) -> (Rect, Image) {
        let mut app = test_app();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let node = brand.build(app.app_mut(), window, root).expect("品牌挂载");
        let _ = app.drain().expect("首帧");
        {
            let tree = app.app().window(window).expect("test window").tree();
            let mounted = tree.get(node).expect("品牌节点");
            assert!(
                matches!(
                    mounted.kind(),
                    NodeKind::Image(content) if content.fit() == ImageFit::Contain
                ),
                "品牌必须是 ImageFit::Contain 的 E7 图片节点：{:?}",
                mounted.kind()
            );
            assert!(
                mounted.children().is_empty(),
                "品牌是图片叶子，不能挂子节点"
            );
        }
        let frame = bounds(&app, node).expect("品牌节点布局盒");
        (frame, assert_single_image(&app))
    }

    /// **Contain 集成**：真实品牌在正方形节点盒里等比内嵌 + 居中 + 留白；正方形品牌/固有尺寸无留白。
    ///
    /// 变异「把 `ImageFit::Contain` 改回 `Fill`」（图元 = 节点盒）⇒ Ethereum 的 22×36 / Solana 的
    /// 36×32 断言变红。
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

        // 正方形品牌：节点盒与图元重合，没有留白（单节点，没有额外容器）。
        let (frame, image) = measured(Brand::new(BrandKind::Binance).with_size(36.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 36.0, 36.0));
        assert_rect_near(image.bounds, Rect::new(0.0, 0.0, 36.0, 36.0));

        // 固有尺寸（不给尺寸）：沿用 PNG 固有宽高，同样不拉伸。
        let (frame, image) = measured(Brand::new(BrandKind::Ethereum));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 44.0, 72.0));
        assert_rect_near(image.bounds, Rect::new(0.0, 0.0, 44.0, 72.0));
    }

    /// **生产尺寸锁**（P2-1b3b / 审查 F8）：14 / 16 px 是菜单里的真实调用尺寸
    /// （`profile_ui/src/menu.rs`、`market_participants_ui/src/menu.rs`），也是相对误差最大的
    /// 地方。E7 `Contain` 按**解码 tile 的整数宽高比**（`fit_into` 两轴 `floor`）计算，因此图元
    /// 比「理论 FP 比例」系统性偏窄 ≤1 逻辑 px —— 这里把已接受的具体值锁住（引擎取整或
    /// Contain 几何回归会让本用例变红）。
    #[test]
    fn production_sizes_14_and_16_letterbox_the_decoded_tile() {
        // Ethereum 44×72 → tile 8×14（floor(44×14/72)=8）；Contain 图元 8×14，左右各留白 3。
        let (frame, image) = measured(Brand::new(BrandKind::Ethereum).with_size(14.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 14.0, 14.0));
        assert_rect_near(image.bounds, Rect::new(3.0, 0.0, 8.0, 14.0));

        // Ethereum 44×72 → tile 9×16（floor(44×16/72)=9）；图元 9×16，左右各留白 3.5。
        let (frame, image) = measured(Brand::new(BrandKind::Ethereum).with_size(16.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 16.0, 16.0));
        assert_rect_near(image.bounds, Rect::new(3.5, 0.0, 9.0, 16.0));

        // Solana 81×72 → tile 14×12（floor(72×14/81)=12）；图元 14×12，上下各留白 1。
        let (frame, image) = measured(Brand::new(BrandKind::Solana).with_size(14.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 14.0, 14.0));
        assert_rect_near(image.bounds, Rect::new(0.0, 1.0, 14.0, 12.0));

        // Solana 81×72 → tile 16×14（floor(72×16/81)=14）；图元 16×14，上下各留白 1。
        let (frame, image) = measured(Brand::new(BrandKind::Solana).with_size(16.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 16.0, 16.0));
        assert_rect_near(image.bounds, Rect::new(0.0, 1.0, 16.0, 14.0));
    }

    /// 挂载：一个 `Image` 图元、不染色（`Color::WHITE`）、节点盒 = 给定尺寸。
    ///
    /// Solana 81×72 放进 32×32：解码变体按物理目标等比 fit 成 **32×28**（整数像素取整，见
    /// `resources::image::resolve_target`），E7 `Contain` 再按 tile 宽高比内嵌 + 居中 ⇒ 32×28
    /// （上下各留白 2）。这是 E7 语义与旧实现（手算 32×28.444 后拉伸 tile）**有意的差异**：
    /// 几何跟随真实解码像素，而不是 FP 理论值。
    #[test]
    fn a_brand_mounts_an_unmodulated_image_node() {
        let (frame, image) = measured(Brand::new(BrandKind::Solana).with_size(32.0));
        assert_rect_near(frame, Rect::new(0.0, 0.0, 32.0, 32.0));
        assert_eq!(image.modulation, Color::WHITE, "品牌图不染色");
        assert_rect_near(image.bounds, Rect::new(0.0, 2.0, 32.0, 28.0));
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

        // `reselect` 只接受图片节点（与 `set_image_resource` 的契约一致）。
        let container = NodeBuilder::boxed()
            .build(
                app.app_mut()
                    .window_mut(window)
                    .expect("test window")
                    .tree_mut(),
                root,
            )
            .expect("容器");
        let error = Brand::new(BrandKind::Binance)
            .reselect(app.app_mut(), window, container)
            .expect_err("非图片节点必须失败");
        assert!(
            matches!(error, BrandError::NotImageNode { node } if node == container),
            "非图片节点的错误分类不对：{error:?}"
        );

        assert!(app.app_mut().close_window(window), "窗口必须存在");
        let error = Brand::new(BrandKind::Binance)
            .build(app.app_mut(), window, root)
            .expect_err("窗口已关闭");
        assert!(matches!(error, BrandError::UnknownWindow { window: closed } if closed == window));
    }

    /// 品牌挂载后场景里只有一张图（回归：E7 单图片叶子不得复制图元）。
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

    /// **a11y**：有 alt ⇒ E7 `Image` 角色 + label（不隐藏、无诊断）；无 alt ⇒ 引擎兜底 hidden，
    /// 并记 `MissingAltText`（提示用 `with_alt` 补可访问名）。
    #[test]
    fn brand_alt_maps_to_the_image_role_and_label() {
        let mut app = test_app();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let labelled = Brand::new(BrandKind::Binance)
            .with_size(16.0)
            .with_alt("Binance")
            .build(app.app_mut(), window, root)
            .expect("有 alt 的品牌挂载");
        let decorative = Brand::new(BrandKind::Ethereum)
            .with_size(16.0)
            .build(app.app_mut(), window, root)
            .expect("无 alt 的品牌挂载");
        let _ = app.drain().expect("首帧");

        let snapshot = app.a11y().expect("a11y 交付");
        let labelled_record = snapshot.node_for(labelled).expect("有 alt 的品牌节点");
        assert_eq!(labelled_record.role, A11yRole::Image, "E7 图片节点角色");
        assert_eq!(
            labelled_record.label_text(),
            Some("Binance"),
            "alt 进 label"
        );
        assert!(!labelled_record.hidden, "有 alt 的品牌图不得隐藏");
        assert!(
            labelled_record.actions.is_empty(),
            "品牌图默认非交互，实际 {:?}",
            labelled_record.actions
        );

        let decorative_record = snapshot.node_for(decorative).expect("无 alt 的品牌节点");
        assert_eq!(decorative_record.role, A11yRole::Image);
        assert!(
            decorative_record.hidden,
            "无 alt 的品牌图按兜底语义 hidden（引擎的 MissingAltText 语义）"
        );

        // 诊断语义（P2-1b3 引擎修正）：显式 hidden 才是装饰声明（Icon 的路径）；Brand 的可访问名
        // 通道是 `with_alt`，无 alt ⇒ 未声明装饰 ⇒ 仍记 MissingAltText 作为补 alt 的提示。
        let diagnostics = app.app().a11y_diagnostics();
        let missing_alt = |node: NodeId| {
            diagnostics.iter().any(|diagnostic| {
                matches!(
                    diagnostic,
                    A11yDiagnostic::MissingAltText {
                        node: target,
                        kind: A11yDrawKind::Image
                    } if *target == node
                )
            })
        };
        assert!(
            !missing_alt(labelled),
            "有 alt 的品牌图不得记 MissingAltText：{diagnostics:?}"
        );
        assert!(
            missing_alt(decorative),
            "无 alt 的品牌图必须记 MissingAltText（提示 with_alt）：{diagnostics:?}"
        );
    }

    /// **DPI 换档**：2× 后 `reselect` 等价于引擎的 `insert_for_display(节点盒, 2×)`；图元换新 tile、
    /// `Contain` 几何与布局盒不变（显式 `Style` 尺寸）。
    ///
    /// 变异防线：`reselect` 不换句柄 / 固定 1× 变体 ⇒ 变体相等与 tile 变化断言变红。
    #[test]
    fn reselecting_after_a_dpi_change_swaps_to_the_2x_variant() {
        let mut app = test_app_with_scale(1.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let brand = Brand::new(BrandKind::Solana).with_size(32.0);
        let node = brand
            .clone()
            .build(app.app_mut(), window, root)
            .expect("品牌挂载");
        let _ = app.drain().expect("1× 首帧");
        let before = assert_single_image(&app);

        set_window_scale(&mut app, 2.0);
        let _ = app.drain().expect("2× 首帧（E7 只重解析内在尺寸）");
        assert_rect_near(
            bounds(&app, node).expect("品牌节点布局盒"),
            Rect::new(0.0, 0.0, 32.0, 32.0),
        );
        assert_eq!(
            assert_single_image(&app).bounds,
            before.bounds,
            "换变体前几何照旧"
        );

        let resource = brand
            .reselect(app.app_mut(), window, node)
            .expect("重选档位");
        let expected = app
            .app_mut()
            .resources_mut()
            .insert_for_display(BrandKind::Solana.bytes(), Size::new(32.0, 32.0), 2.0)
            .expect("引擎的 E7 显示变体");
        assert_eq!(
            resource, expected,
            "reselect 必须等价于 insert_for_display(节点盒, 2×)"
        );
        assert!(
            resource.width > 32 || resource.height > 32,
            "2× 变体的物理像素必须比 1× 更大：{resource:?}"
        );

        let _ = app.drain().expect("换变体后一帧");
        let after = assert_single_image(&app);
        assert_eq!(after.tile, resource.tile, "图元必须用 2× 变体的 tile");
        assert_ne!(after.tile, before.tile, "换档必须换到另一张 tile");
        // 解码变体：1× = 32×28、2× = 64×56（`fit_into` 等比 + floor 取整），宽高比同为 8:7 ⇒
        // E7 `Contain` 的图元几何在两个 DPI 档一致（32×28，上下各留白 2）。
        assert_rect_near(after.bounds, Rect::new(0.0, 2.0, 32.0, 28.0));
    }

    /// 改窗口 DPI（`Window::set_scale` 递增 `scale_epoch`；E7 下一帧布局前重解析内在尺寸）。
    fn set_window_scale(app: &mut TestApp, scale: f32) {
        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("test window")
            .set_scale(Scale::new(scale).expect("测试 DPI 必须是有限正数"));
    }

    /// G7（P3，登记项）：品牌 PNG 是**彩色**资产、`Brand` 不做主题调制/背景适配 —— 旧
    /// `crate::brand` 同样只是 `img(path)`，所以这不是迁移回归；正确修法是资产批次补浅色变体
    /// （kit 侧没有动态反色通道），示例用固定深色底板承载。
    ///
    /// 本用例把该决策固定下来：外观切换**不失效**品牌节点、调制色恒为 `Color::WHITE`。
    /// 变异（给品牌加主题绑定/调制）⇒ 本例变红。
    #[test]
    fn a_brand_logo_is_theme_independent() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let node = Brand::new(BrandKind::Ethereum)
            .with_size(26.0)
            .with_alt("Ethereum")
            .build(app.app_mut(), window, root)
            .expect("品牌挂载");
        let _ = app.drain().expect("首帧");

        assert!(theme.set_appearance(app.app_mut(), Appearance::Dark));
        let record = app.drain().expect("切深色").clone();
        assert!(
            !record.dirty_nodes.contains(&node),
            "品牌图不读主题（G7：kit 不做主题适配）：{:?}",
            record.dirty_nodes
        );
        let images = image_primitives(&app);
        assert_eq!(
            images.first().map(|image| image.modulation),
            Some(Color::WHITE),
            "品牌图调制色恒为白（不染色）"
        );
    }
}

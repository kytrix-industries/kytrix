//! 品牌 logo：7 个内置品牌的路径 / 字节 / 固有尺寸，以及图片节点挂载。
//!
//! # 与旧实现的关系
//!
//! 旧 `crate::brand` 是「7 条路径常量 + 一个 `Brand` 元素（`img(path)`）」：路径常量给
//! **命名者**（Widget 占位文案里的内联 `<img>`）用，`Brand` 给**画图者**用。两条用途在这里
//! 合成一份数据：`BrandKind` 是唯一事实源（路径 / 字节 / 固有尺寸都从它取），`Brand` 负责把
//! logo 挂成图片节点。
//!
//! 字节来自 [`crate::kit::assets`] 的注册表（`include_bytes!`）；运行期用引擎
//! `ResourceCache::insert_owned_at_size` 取变体（同内容 + 同变体只占一个 tile）。品牌 logo 是
//! **彩色** PNG，调制色恒为 `Color::WHITE`（不染色）；白字调制的图片语义见
//! [`crate::kit::icon`] 的模块文档。
//!
//! 合规口径沿用旧注释：`Longbridge` 的 PNG 是占位图，正式资产要在取得展示权后替换；路径不变
//! （设计 §8.5 的合规缺口只登记，不在本批范围）。

use std::fmt;

use ui_core::{
    app::App, error::EngineError, math::Size, node::id::NodeId, paint::color::Color,
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
    /// 逻辑边长（正方形；`None` = 用固有宽高，保持长宽比）。
    ///
    /// 旧的 gpui `.size_6()` 等写法会把非正方形 logo **拉伸**成正方形（GPUI `img()` 默认
    /// `object-fit: fill`），所以这里显式给尺寸时也是正方形拉伸，与旧行为一致。
    pub size: Option<f32>,
}

impl Brand {
    /// 固有尺寸的 logo。
    #[must_use]
    pub const fn new(kind: BrandKind) -> Self {
        Self { kind, size: None }
    }

    /// 正方形边长（逻辑像素）。
    #[must_use]
    pub const fn with_size(mut self, side: f32) -> Self {
        self.size = Some(side);
        self
    }

    /// 实际绘制尺寸（逻辑像素）。
    #[must_use]
    pub fn display_size(self) -> Size {
        match self.size {
            Some(side) => Size::new(side, side),
            None => self.kind.native_size(),
        }
    }

    /// 把 logo 挂到 `parent` 末尾，返回图片节点句柄。
    ///
    /// 与 [`crate::kit::icon::IconSpec::build`] 同一条图片通道（行内盒图片），差别只有：
    /// 资源是**彩色**品牌 PNG、调制色恒为白色、字节来自 [`crate::kit::assets`] 注册表。
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
        let display = self.display_size();
        let physical = Size::new(display.width * scale, display.height * scale);
        let resource = app
            .resources_mut()
            .insert_owned_at_size(window, self.kind.bytes(), physical)
            .map_err(BrandError::Decode)?;
        image_node::mount(app, window, parent, resource.id, display, Color::WHITE)
            .map_err(BrandError::Node)
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
    use ui_core::math::Rect;

    use super::{Brand, BrandError, BrandKind};
    use crate::kit::test_support::{image_primitives, test_app_with_scale};

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

    /// 固有尺寸保真（Ethereum / Solana 是非正方形），显式尺寸是正方形拉伸（旧 `.size(px)` 语义）。
    #[test]
    fn native_sizes_keep_the_aspect_ratio_and_explicit_sizes_are_square() {
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

    /// 挂载：一个 `Image` 图元、不染色（`Color::WHITE`）、布局盒 = 给定尺寸。
    #[test]
    fn a_brand_mounts_an_unmodulated_image_node() {
        let mut app = test_app_with_scale(2.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let node = Brand::new(BrandKind::Solana)
            .with_size(32.0)
            .build(app.app_mut(), window, root)
            .expect("品牌挂载");
        let _ = app.drain().expect("首帧");

        assert_eq!(
            app.probe().bounds(node),
            Some(Rect::new(0.0, 0.0, 32.0, 32.0))
        );
        let images = image_primitives(&app);
        assert_eq!(images.len(), 1, "一个品牌只应产生一个 Image 图元");
        assert_eq!(
            images[0].modulation,
            ui_core::paint::color::Color::WHITE,
            "品牌图不染色"
        );
    }

    /// 非法尺寸 / 已关闭窗口是强类型错误。
    #[test]
    fn brand_errors_are_typed() {
        let mut app = test_app_with_scale(1.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let error = Brand::new(BrandKind::Binance)
            .with_size(0.0)
            .build(app.app_mut(), window, root)
            .expect_err("非法尺寸");
        assert!(
            matches!(&error, BrandError::InvalidSize { size } if *size == 0.0),
            "非法尺寸的错误分类不对：{error:?}"
        );

        assert!(app.app_mut().close_window(window), "窗口必须存在");
        let error = Brand::new(BrandKind::Binance)
            .build(app.app_mut(), window, root)
            .expect_err("窗口已关闭");
        assert!(matches!(error, BrandError::UnknownWindow { window: closed } if closed == window));
    }
}

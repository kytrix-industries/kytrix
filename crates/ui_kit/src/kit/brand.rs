use std::{fmt, sync::OnceLock};

use smol_str::SmolStr;
use ui_core::{
    a11y::A11yOverride,
    app::App,
    error::EngineError,
    math::{LogicalPx, LogicalSize},
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit},
    paint::{color::Color, style::Style},
    resources::ResourceImage,
    window::WindowId,
};

use crate::kit::assets;

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

pub const ETHEREUM: &str = "brands/Ethereum/colored@72px.png";

pub const SOLANA: &str = "brands/Solana/colored@72px.png";

pub const BINANCE: &str = "brands/Binance/binance.png";

pub const OKX: &str = "brands/OKX/okx.png";

pub const HYPERLIQUID: &str = "brands/Hyperliquid/hyperliquid.png";

pub const POLYMARKET: &str = "brands/Polymarket/polymarket.png";

pub const LONGBRIDGE: &str = "brands/Longbridge/longbridge.png";

impl BrandKind {
    pub const ALL: [BrandKind; 7] = [
        BrandKind::Binance,
        BrandKind::Ethereum,
        BrandKind::Hyperliquid,
        BrandKind::Longbridge,
        BrandKind::Okx,
        BrandKind::Polymarket,
        BrandKind::Solana,
    ];

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

    #[must_use]
    pub const fn asset_path(self) -> &'static str {
        match self {
            BrandKind::Binance => BINANCE,
            BrandKind::Ethereum => ETHEREUM,
            BrandKind::Hyperliquid => HYPERLIQUID,
            BrandKind::Longbridge => LONGBRIDGE,
            BrandKind::Okx => OKX,
            BrandKind::Polymarket => POLYMARKET,
            BrandKind::Solana => SOLANA,
        }
    }

    #[must_use]
    pub fn bytes(self) -> &'static [u8] {
        assets::bytes(self.asset_path()).expect("品牌 PNG 必须登记在 kit::assets::ASSETS 里")
    }

    #[must_use]
    pub fn native_size(self) -> LogicalSize {
        static NATIVE_SIZES: OnceLock<[LogicalSize; BrandKind::ALL.len()]> = OnceLock::new();
        let sizes = NATIVE_SIZES.get_or_init(|| {
            BrandKind::ALL.map(|kind| {
                assets::find(kind.asset_path())
                    .and_then(assets::Asset::native_size)
                    .expect("品牌 PNG 必须是可读文件头的位图（PNG）")
            })
        });
        let index = BrandKind::ALL
            .iter()
            .position(|kind| *kind == self)
            .expect("BrandKind::ALL 覆盖全部变体");
        sizes[index]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Brand {
    pub kind: BrandKind,
    pub size: Option<LogicalPx>,
    pub alt: Option<SmolStr>,
}

impl Brand {
    #[must_use]
    pub const fn new(kind: BrandKind) -> Self {
        Self {
            kind,
            size: None,
            alt: None,
        }
    }

    #[must_use]
    pub const fn with_size(mut self, side: LogicalPx) -> Self {
        self.size = Some(side);
        self
    }

    #[must_use]
    pub fn with_alt(mut self, alt: impl Into<SmolStr>) -> Self {
        self.alt = Some(alt.into());
        self
    }

    #[must_use]
    pub fn display_size(&self) -> LogicalSize {
        match self.size {
            Some(side) => LogicalSize::new(side, side),
            None => self.kind.native_size(),
        }
    }

    pub fn build(
        self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, BrandError> {
        if let Some(side) = self.size
            && (!side.is_finite() || side <= LogicalPx::ZERO)
        {
            return Err(BrandError::InvalidSize { size: side });
        }
        let frame = self.display_size();
        let resource = self.resource_for(app, window)?;

        let mut style = Style::default();
        style.size_length(frame.width, frame.height);
        let mut builder = NodeBuilder::image(resource.id(), ImageFit::Contain)
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

    fn resource_for(&self, app: &mut App, window: WindowId) -> Result<ResourceImage, BrandError> {
        app.insert_bitmap_for_window(window, self.kind.bytes())
            .map_err(|error| match error {
                EngineError::UnknownWindow { window } => BrandError::UnknownWindow { window },
                other => BrandError::Decode(other),
            })
    }
}

#[derive(Debug)]
pub enum BrandError {
    UnknownWindow { window: WindowId },
    InvalidSize { size: LogicalPx },
    Decode(EngineError),
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
        a11y::{A11yDiagnostic, A11yDrawKind, A11yRole},
        math::{DevicePixelRatio, LogicalRect, px},
        node::{id::NodeId, image::ImageFit, tree::NodeKind},
        paint::{color::Color, primitive::Image},
        resources::{ImageResourceKind, ResourceImage},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{
        BINANCE, Brand, BrandError, BrandKind, ETHEREUM, HYPERLIQUID, LONGBRIDGE, OKX, POLYMARKET,
        SOLANA,
    };
    use crate::{
        kit::test_support::{assert_single_image, bounds, image_primitives, test_app},
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    #[test]
    fn every_brand_matches_the_legacy_paths_and_bytes() {
        assert_eq!(BrandKind::ALL.len(), 7);
        let legacy = [
            (
                BrandKind::Ethereum,
                "brands/Ethereum/colored@72px.png",
                ETHEREUM,
            ),
            (BrandKind::Solana, "brands/Solana/colored@72px.png", SOLANA),
            (BrandKind::Binance, "brands/Binance/binance.png", BINANCE),
            (BrandKind::Okx, "brands/OKX/okx.png", OKX),
            (
                BrandKind::Hyperliquid,
                "brands/Hyperliquid/hyperliquid.png",
                HYPERLIQUID,
            ),
            (
                BrandKind::Polymarket,
                "brands/Polymarket/polymarket.png",
                POLYMARKET,
            ),
            (
                BrandKind::Longbridge,
                "brands/Longbridge/longbridge.png",
                LONGBRIDGE,
            ),
        ];
        for (kind, path, constant) in legacy {
            assert_eq!(kind.asset_path(), path, "{kind:?} 的路径与旧常量不一致");
            assert_eq!(constant, path, "{kind:?} 的公开常量与旧路径不一致");
            let registered = crate::kit::assets::bytes(path).expect("注册表里必须有同名文件");
            assert_eq!(kind.bytes(), registered, "{kind:?} 的字节漂移");
        }
        let mut names: Vec<&str> = BrandKind::ALL.iter().map(|kind| kind.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 7, "品牌名必须唯一");
    }

    #[test]
    fn native_sizes_keep_the_aspect_ratio_and_explicit_sizes_are_square_boxes() {
        assert_eq!(
            BrandKind::Ethereum.native_size(),
            ui_core::math::LogicalSize::new(px(44.0), px(72.0))
        );
        assert_eq!(
            BrandKind::Solana.native_size(),
            ui_core::math::LogicalSize::new(px(81.0), px(72.0))
        );
        assert_eq!(
            Brand::new(BrandKind::Ethereum).display_size(),
            ui_core::math::LogicalSize::new(px(44.0), px(72.0))
        );
        assert_eq!(
            Brand::new(BrandKind::Ethereum)
                .with_size(px(24.0))
                .display_size(),
            ui_core::math::LogicalSize::new(px(24.0), px(24.0))
        );
    }

    fn assert_rect_near(actual: LogicalRect, expected: LogicalRect) {
        let eps = 1e-2;
        assert!(
            (actual.origin.x - expected.origin.x).abs() < px(eps)
                && (actual.origin.y - expected.origin.y).abs() < px(eps)
                && (actual.size.width - expected.size.width).abs() < px(eps)
                && (actual.size.height - expected.size.height).abs() < px(eps),
            "期望 {expected:?}，实际 {actual:?}"
        );
    }

    fn measured(brand: Brand) -> (LogicalRect, Image) {
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

    #[test]
    fn non_square_brands_are_contained_in_the_requested_square() {
        let (frame, image) = measured(Brand::new(BrandKind::Ethereum).with_size(px(36.0)));
        assert_rect_near(
            frame,
            LogicalRect::new(px(0.0), px(0.0), px(36.0), px(36.0)),
        );
        assert_rect_near(
            image.bounds,
            LogicalRect::new(px(7.0), px(0.0), px(22.0), px(36.0)),
        );
        assert_eq!(image.modulation, Color::WHITE, "品牌图不染色");

        let (frame, image) = measured(Brand::new(BrandKind::Solana).with_size(px(36.0)));
        assert_rect_near(
            frame,
            LogicalRect::new(px(0.0), px(0.0), px(36.0), px(36.0)),
        );
        assert_rect_near(
            image.bounds,
            LogicalRect::new(px(0.0), px(2.0), px(36.0), px(32.0)),
        );

        let (frame, image) = measured(Brand::new(BrandKind::Binance).with_size(px(36.0)));
        assert_rect_near(
            frame,
            LogicalRect::new(px(0.0), px(0.0), px(36.0), px(36.0)),
        );
        assert_rect_near(
            image.bounds,
            LogicalRect::new(px(0.0), px(0.0), px(36.0), px(36.0)),
        );

        let (frame, image) = measured(Brand::new(BrandKind::Ethereum));
        assert_rect_near(
            frame,
            LogicalRect::new(px(0.0), px(0.0), px(44.0), px(72.0)),
        );
        assert_rect_near(
            image.bounds,
            LogicalRect::new(px(0.0), px(0.0), px(44.0), px(72.0)),
        );
    }

    #[test]
    fn production_sizes_14_and_16_letterbox_the_native_aspect() {
        let (frame, image) = measured(Brand::new(BrandKind::Ethereum).with_size(px(14.0)));
        assert_rect_near(
            frame,
            LogicalRect::new(px(0.0), px(0.0), px(14.0), px(14.0)),
        );
        assert_rect_near(
            image.bounds,
            LogicalRect::new(px(2.722_222_3), px(0.0), px(8.555_555), px(14.0)),
        );

        let (frame, image) = measured(Brand::new(BrandKind::Ethereum).with_size(px(16.0)));
        assert_rect_near(
            frame,
            LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)),
        );
        assert_rect_near(
            image.bounds,
            LogicalRect::new(px(3.111_111_2), px(0.0), px(9.777_778), px(16.0)),
        );

        let (frame, image) = measured(Brand::new(BrandKind::Solana).with_size(px(14.0)));
        assert_rect_near(
            frame,
            LogicalRect::new(px(0.0), px(0.0), px(14.0), px(14.0)),
        );
        assert_rect_near(
            image.bounds,
            LogicalRect::new(px(0.0), px(0.777_777_8), px(14.0), px(12.444_445)),
        );

        let (frame, image) = measured(Brand::new(BrandKind::Solana).with_size(px(16.0)));
        assert_rect_near(
            frame,
            LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)),
        );
        assert_rect_near(
            image.bounds,
            LogicalRect::new(px(0.0), px(0.888_888_9), px(16.0), px(14.222_223)),
        );
    }

    #[test]
    fn a_brand_mounts_an_unmodulated_image_node() {
        let (frame, image) = measured(Brand::new(BrandKind::Solana).with_size(px(32.0)));
        assert_rect_near(
            frame,
            LogicalRect::new(px(0.0), px(0.0), px(32.0), px(32.0)),
        );
        assert_eq!(image.modulation, Color::WHITE, "品牌图不染色");
        assert_rect_near(
            image.bounds,
            LogicalRect::new(px(0.0), px(1.777_777_7), px(32.0), px(28.444_445)),
        );
    }

    #[test]
    fn brand_errors_are_typed() {
        let mut app = test_app();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let side = px(size);
            let error = Brand::new(BrandKind::Binance)
                .with_size(side)
                .build(app.app_mut(), window, root)
                .expect_err("非法尺寸必须失败");
            assert!(
                matches!(&error, BrandError::InvalidSize { size: actual } if actual.value().to_bits() == size.to_bits()),
                "size={size} 的错误分类不对：{error:?}"
            );
        }

        assert!(app.app_mut().close_window(window), "窗口必须存在");
        let error = Brand::new(BrandKind::Binance)
            .build(app.app_mut(), window, root)
            .expect_err("窗口已关闭");
        assert!(matches!(error, BrandError::UnknownWindow { window: closed } if closed == window));
    }

    #[test]
    fn a_letterboxed_brand_still_encodes_exactly_one_image_primitive() {
        let mut app = test_app();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        Brand::new(BrandKind::Ethereum)
            .with_size(px(36.0))
            .build(app.app_mut(), window, root)
            .expect("品牌挂载");
        let _ = app.drain().expect("首帧");
        assert_eq!(image_primitives(&app).len(), 1);
    }

    #[test]
    fn brand_alt_maps_to_the_image_role_and_label() {
        let mut app = test_app();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let labelled = Brand::new(BrandKind::Binance)
            .with_size(px(16.0))
            .with_alt("Binance")
            .build(app.app_mut(), window, root)
            .expect("有 alt 的品牌挂载");
        let decorative = Brand::new(BrandKind::Ethereum)
            .with_size(px(16.0))
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

    #[test]
    fn a_brand_registers_its_bitmap_at_the_native_pixels() {
        let mut app = test_app();
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let node = Brand::new(BrandKind::Solana)
            .with_size(px(32.0))
            .build(app.app_mut(), window, root)
            .expect("品牌挂载");
        let _ = app.drain().expect("首帧");

        let image = resource_of(&app, window, node);
        assert!(
            matches!(image.kind(), ImageResourceKind::Bitmap { .. }),
            "品牌是位图资源：{:?}",
            image.kind()
        );
        assert_eq!(
            image.intrinsic(DevicePixelRatio::ONE),
            BrandKind::Solana.native_size(),
            "位图按原生像素登记，内在逻辑尺寸 = 原生 / DPR"
        );
        let tree = app.app().window(window).expect("test window").tree();
        let mounted = tree.get(node).expect("品牌节点");
        let NodeKind::Image(content) = mounted.kind() else {
            panic!("品牌必须是图片节点");
        };
        assert_eq!(
            content.intrinsic(),
            Some(BrandKind::Solana.native_size()),
            "节点内在尺寸由资源导出，与 32 px 显示盒无关"
        );
    }

    fn resource_of(app: &TestApp, window: WindowId, node: NodeId) -> ResourceImage {
        let tree = app.app().window(window).expect("test window").tree();
        let NodeKind::Image(content) = tree.get(node).expect("品牌节点").kind() else {
            panic!("品牌必须是图片节点");
        };
        let id = content.source().resource().expect("品牌是就绪资源");
        app.app().resources().image(id).expect("资源必须仍然存活")
    }

    #[test]
    fn a_brand_logo_is_theme_independent() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("test window").tree().root();
        let node = Brand::new(BrandKind::Ethereum)
            .with_size(px(26.0))
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

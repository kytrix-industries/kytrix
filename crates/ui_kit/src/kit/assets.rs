use std::fmt;

use ahash::HashMap;
use bytes::Bytes;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    math::Size,
    node::image::ImageSource,
    resources::{DecodeCause, DecodeState, PendingResource, ResourceImage},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AssetKind {
    Brand,
    WidgetPreview,
}

#[derive(Clone, Copy, Debug)]
pub struct Asset {
    pub path: &'static str,
    pub kind: AssetKind,
    pub bytes: &'static [u8],
}

impl Asset {
    #[must_use]
    pub fn native_size(&self) -> Option<Size> {
        let (width, height) = image::ImageReader::new(std::io::Cursor::new(self.bytes))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()?;
        Some(Size::new(width as f32, height as f32))
    }
}

macro_rules! asset_table {
    ($($kind:ident => $path:literal),* $(,)?) => {
        pub const ASSETS: &[Asset] = &[
            $(
                Asset {
                    path: $path,
                    kind: AssetKind::$kind,
                    bytes: include_bytes!(concat!("../../assets/", $path)),
                },
            )*
        ];
    };
}

asset_table! {
    Brand => "brands/Ethereum/colored@72px.png",
    Brand => "brands/Solana/colored@72px.png",
    Brand => "brands/Binance/binance.png",
    Brand => "brands/OKX/okx.png",
    Brand => "brands/Hyperliquid/hyperliquid.png",
    Brand => "brands/Polymarket/polymarket.png",
    Brand => "brands/Longbridge/longbridge.png",
    WidgetPreview => "widgets/candlestick.png",
    WidgetPreview => "widgets/coin_market_cap_fear_and_greed_index.png",
    WidgetPreview => "widgets/cryptocurrency_etf.png",
    WidgetPreview => "widgets/ethereum_wallet.png",
    WidgetPreview => "widgets/financial_calendar.png",
    WidgetPreview => "widgets/force_order.png",
    WidgetPreview => "widgets/global_shipping_chokepoint.png",
    WidgetPreview => "widgets/hyperliquid_wallet.png",
    WidgetPreview => "widgets/indices.png",
    WidgetPreview => "widgets/long_short_account_ratio.png",
    WidgetPreview => "widgets/market_trade.png",
    WidgetPreview => "widgets/news.png",
    WidgetPreview => "widgets/open_interest.png",
    WidgetPreview => "widgets/order_book.png",
    WidgetPreview => "widgets/pentagon_pizza_index.png",
    WidgetPreview => "widgets/prediction_market.png",
    WidgetPreview => "widgets/solana_wallet.png",
    WidgetPreview => "widgets/taker_volume.png",
    WidgetPreview => "widgets/top_trader_account_ratio.png",
    WidgetPreview => "widgets/top_trader_position_ratio.png",
    WidgetPreview => "widgets/volume_heatmap.png",
    WidgetPreview => "widgets/watch_list.png",
    WidgetPreview => "widgets/x.png",
}

#[must_use]
pub fn find(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

#[must_use]
pub fn bytes(path: &str) -> Option<&'static [u8]> {
    find(path).map(|asset| asset.bytes)
}

pub fn list(prefix: &str) -> impl Iterator<Item = &'static str> {
    ASSETS
        .iter()
        .filter(move |asset| asset.path.starts_with(prefix))
        .map(|asset| asset.path)
}

#[derive(Clone, Debug, PartialEq)]
pub enum AssetImageState {
    Pending(PendingResource),
    Ready(ResourceImage),
    Failed(DecodeCause),
}

impl AssetImageState {
    #[must_use]
    pub fn node_source(&self) -> Option<ImageSource> {
        match self {
            AssetImageState::Pending(pending) => Some(ImageSource::Pending(*pending)),
            AssetImageState::Ready(image) => Some(ImageSource::Ready(image.id)),
            AssetImageState::Failed(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssetLoaded {
    pub path: SmolStr,
    pub state: AssetImageState,
}

#[derive(Debug, Default)]
pub struct AssetImages {
    entries: HashMap<SmolStr, AssetImageState>,
    pending: HashMap<PendingResource, SmolStr>,
}

impl AssetImages {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(
        &mut self,
        app: &mut App,
        path: &str,
        display: Size,
    ) -> Result<AssetImageState, AssetError> {
        let asset = find(path).ok_or_else(|| AssetError::UnknownAsset {
            path: SmolStr::new(path),
        })?;
        if let Some(state) = self.entries.get(path) {
            return Ok(state.clone());
        }
        let pending = app
            .resources_mut()
            .insert_async_at_size(Bytes::from_static(asset.bytes), display);
        let path = SmolStr::new(path);
        self.pending.insert(pending, path.clone());
        let state = AssetImageState::Pending(pending);
        self.entries.insert(path, state.clone());
        Ok(state)
    }

    pub fn request_source(
        &mut self,
        app: &mut App,
        path: &str,
        display: Size,
    ) -> Result<Option<ImageSource>, AssetError> {
        let state = self.request(app, path, display)?;
        Ok(state.node_source())
    }

    pub fn poll(&mut self, app: &mut App) -> Vec<AssetLoaded> {
        Self::poll_all(app, &mut [self]).pop().unwrap_or_default()
    }

    pub fn poll_all(app: &mut App, caches: &mut [&mut AssetImages]) -> Vec<Vec<AssetLoaded>> {
        let results = app.take_resource_results();
        let mut delivered = vec![Vec::new(); caches.len()];
        for (pending, state) in results {
            let Some(index) = caches
                .iter()
                .position(|cache| cache.pending.contains_key(&pending))
            else {
                continue;
            };
            if let Some(loaded) = caches[index].accept(pending, state) {
                delivered[index].push(loaded);
            }
        }
        delivered
    }

    fn accept(&mut self, pending: PendingResource, state: DecodeState) -> Option<AssetLoaded> {
        let path = self.pending.remove(&pending)?;
        let state = match state {
            DecodeState::Pending => AssetImageState::Pending(pending),
            DecodeState::Ready(image) => AssetImageState::Ready(image),
            DecodeState::Failed(cause) => AssetImageState::Failed(cause),
        };
        self.entries.insert(path.clone(), state.clone());
        Some(AssetLoaded { path, state })
    }

    #[must_use]
    pub fn state(&self, path: &str) -> Option<&AssetImageState> {
        self.entries.get(path)
    }

    #[must_use]
    pub fn node_source(&self, path: &str) -> Option<ImageSource> {
        self.entries.get(path)?.node_source()
    }

    #[must_use]
    pub fn image(&self, path: &str) -> Option<ResourceImage> {
        match self.entries.get(path)? {
            AssetImageState::Ready(image) => Some(*image),
            AssetImageState::Pending(_) | AssetImageState::Failed(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AssetError {
    UnknownAsset { path: SmolStr },
}

impl fmt::Display for AssetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AssetError::UnknownAsset { path } => {
                write!(formatter, "unknown asset `{path}`")
            }
        }
    }
}

impl std::error::Error for AssetError {}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use smol_str::SmolStr;
    use ui_core::{
        app::App,
        math::{Rect, Size},
        node::{
            builder::NodeBuilder,
            image::{ImageFit, ImageSource},
            tree::NodeKind,
        },
        paint::{primitive::Primitive, style::Style},
        resources::ResourceImage,
        window::WindowId,
    };

    use super::{
        ASSETS, AssetError, AssetImageState, AssetImages, AssetKind, AssetLoaded, bytes, find, list,
    };
    use crate::kit::test_support::DecodeApp;

    #[test]
    fn the_registry_is_unique_and_only_holds_pngs() {
        assert_eq!(ASSETS.len(), 30, "7 个品牌 + 23 张 Widget 预览图");
        let mut paths: Vec<&'static str> = ASSETS.iter().map(|asset| asset.path).collect();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), ASSETS.len(), "资产路径必须唯一");
        assert_eq!(
            ASSETS
                .iter()
                .filter(|asset| asset.kind == AssetKind::Brand)
                .count(),
            7
        );
        assert_eq!(
            ASSETS
                .iter()
                .filter(|asset| asset.kind == AssetKind::WidgetPreview)
                .count(),
            23
        );
        for asset in ASSETS {
            assert_eq!(find(asset.path).map(|found| found.path), Some(asset.path));
            assert_eq!(bytes(asset.path), Some(asset.bytes));
            assert!(
                matches!(
                    image::guess_format(asset.bytes),
                    Ok(image::ImageFormat::Png)
                ),
                "`{}` 不是 PNG",
                asset.path
            );
            assert!(!asset.bytes.is_empty());
        }
        assert!(find("widgets/no-such-widget.png").is_none());
        assert!(bytes("widgets/no-such-widget.png").is_none());
    }

    #[test]
    fn list_filters_by_prefix() {
        let brands: Vec<&str> = list("brands/").collect();
        let previews: Vec<&str> = list("widgets/").collect();
        assert_eq!(brands.len(), 7);
        assert_eq!(previews.len(), 23);
        assert_eq!(list("brands/Ethereum/").count(), 1);
        assert_eq!(list("no-such-dir/").count(), 0);
        assert_eq!(list("").count(), ASSETS.len());
    }

    #[test]
    fn native_sizes_come_from_the_file_header() {
        let ethereum = find("brands/Ethereum/colored@72px.png").expect("Ethereum");
        assert_eq!(ethereum.native_size(), Some(Size::new(44.0, 72.0)));
        let solana = find("brands/Solana/colored@72px.png").expect("Solana");
        assert_eq!(solana.native_size(), Some(Size::new(81.0, 72.0)));
        let binance = find("brands/Binance/binance.png").expect("Binance");
        assert_eq!(binance.native_size(), Some(Size::new(128.0, 128.0)));
    }

    #[test]
    fn async_asset_decoding_is_delivered_by_the_engine_poll() {
        let mut harness = DecodeApp::new();
        let mut images = AssetImages::new();
        let path = "brands/Hyperliquid/hyperliquid.png";
        let state = images
            .request(harness.app_mut(), path, Size::new(24.0, 24.0))
            .expect("已登记的资产");
        let AssetImageState::Pending(pending) = state else {
            panic!("首次请求必须是 Pending，实际 {state:?}");
        };
        assert_eq!(
            images.node_source(path),
            Some(ImageSource::Pending(pending)),
            "Pending 状态必须携带可挂 E7 图片节点的在途句柄"
        );

        let loaded = harness.poll_until(600, |app| {
            let loaded = images.poll(app);
            (!loaded.is_empty()).then_some(loaded)
        });
        let loaded = loaded.expect("异步解码必须在 ≈3 s 内交付");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, path);
        let AssetImageState::Ready(image) = loaded[0].state else {
            panic!("Hyperliquid logo 应解码成功，实际是 {:?}", loaded[0].state);
        };
        assert_eq!((image.width, image.height), (24, 24));
        assert_eq!(
            images.image(path),
            Some(ResourceImage {
                id: image.id,
                tile: image.tile,
                width: 24,
                height: 24,
            })
        );
        assert_eq!(
            images.node_source(path),
            Some(ImageSource::Ready(image.id)),
            "Ready 状态必须给出 E7 图片节点的资源句柄"
        );

        assert!(images.poll(harness.app_mut()).is_empty());
        assert_eq!(
            images
                .request(harness.app_mut(), path, Size::new(24.0, 24.0))
                .expect("已登记"),
            AssetImageState::Ready(image)
        );
        assert_eq!(
            images
                .request_source(harness.app_mut(), path, Size::new(24.0, 24.0))
                .expect("已登记"),
            Some(ImageSource::Ready(image.id)),
            "去重命中时 request_source 必须直接给出 Ready 来源"
        );
    }

    #[test]
    fn a_failed_decode_has_no_image_source() {
        let mut harness = DecodeApp::new();
        let mut images = AssetImages::new();
        let path = "widgets/news.png";
        let pending = harness
            .app_mut()
            .resources_mut()
            .insert_async_at_size(Bytes::from_static(b"not a png"), Size::new(8.0, 8.0));
        images.pending.insert(pending, SmolStr::new_static(path));
        images
            .entries
            .insert(SmolStr::new_static(path), AssetImageState::Pending(pending));

        let loaded = harness
            .poll_until(600, |app| {
                let loaded = images.poll(app);
                (!loaded.is_empty()).then_some(loaded)
            })
            .expect("失败结果也必须经引擎交付");
        assert_eq!(loaded.len(), 1);
        assert!(
            matches!(loaded[0].state, AssetImageState::Failed(_)),
            "无法解析的字节必须交付 Failed，实际 {:?}",
            loaded[0].state
        );
        assert_eq!(images.state(path), Some(&loaded[0].state));
        assert_eq!(
            images.node_source(path),
            None,
            "Failed 状态没有可挂的 E7 图片来源"
        );
        assert_eq!(
            images
                .request_source(harness.app_mut(), path, Size::new(8.0, 8.0))
                .expect("已登记的路径"),
            None,
            "Failed 后 request_source 不得重新投递或给出来源"
        );
    }

    #[test]
    fn a_pending_asset_node_repaints_when_the_decode_arrives() {
        let mut harness = DecodeApp::new();
        let mut images = AssetImages::new();
        let path = "brands/Binance/binance.png";
        let source = images
            .request_source(harness.app_mut(), path, Size::new(24.0, 24.0))
            .expect("已登记的资产")
            .expect("首次请求必须是可挂来源（Pending）");
        let ImageSource::Pending(pending) = source else {
            panic!("首次请求必须是 Pending（异步宿主），实际 {source:?}");
        };

        let window = harness.window_id();
        let root = harness.app().window(window).expect("窗口").tree().root();
        let mut style = Style::default();
        style.size_length(24.0, 24.0);
        let node = NodeBuilder::image_pending(pending, ImageFit::Contain)
            .style(style)
            .build(
                harness
                    .app_mut()
                    .window_mut(window)
                    .expect("窗口")
                    .tree_mut(),
                root,
            )
            .expect("挂 E7 占位图片节点");
        assert!(
            matches!(
                image_source(&harness, window, node),
                Some(ImageSource::Pending(handle)) if handle == pending
            ),
            "挂载时节点必须持有 Pending 句柄"
        );

        let loaded = harness
            .poll_until(600, |app| {
                let loaded = images.poll(app);
                (!loaded.is_empty()).then_some(loaded)
            })
            .expect("异步解码必须在 ≈3 s 内交付");
        assert!(matches!(loaded[0].state, AssetImageState::Ready(_)));
        let _ = harness.app_mut().frame(window).expect("交付后一帧");

        assert!(
            matches!(
                image_source(&harness, window, node),
                Some(ImageSource::Ready(_))
            ),
            "引擎必须把节点从 Pending 自动改写为 Ready（不需要应用重建节点）"
        );
        assert!(matches!(
            images.node_source(path),
            Some(ImageSource::Ready(_))
        ));
        let painted = scene_images(harness.app(), window);
        assert_eq!(painted.len(), 1, "资源到达后必须画出图片图元：{painted:?}");
        assert_eq!(
            painted[0].bounds,
            Rect::new(0.0, 0.0, 24.0, 24.0),
            "Binance 是正方形，Contain 填满正方形节点盒"
        );
    }

    fn image_source(
        harness: &DecodeApp,
        window: WindowId,
        node: ui_core::node::id::NodeId,
    ) -> Option<ImageSource> {
        let tree = harness.app().window(window).expect("窗口").tree();
        match tree.get(node).expect("节点必须存活").kind() {
            NodeKind::Image(content) => Some(content.source()),
            _ => None,
        }
    }

    fn scene_images(app: &App, window: WindowId) -> Vec<ui_core::paint::primitive::Image> {
        app.window(window)
            .expect("窗口")
            .scene()
            .iter()
            .filter_map(|primitive| match primitive {
                Primitive::Image(image) => Some(*image),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn two_caches_share_the_delivery_buffer_without_swallowing() {
        let mut harness = DecodeApp::new();
        let mut first = AssetImages::new();
        let mut second = AssetImages::new();
        let first_path = "brands/Hyperliquid/hyperliquid.png";
        let second_path = "brands/Binance/binance.png";
        first
            .request(harness.app_mut(), first_path, Size::new(24.0, 24.0))
            .expect("已登记的资产");
        second
            .request(harness.app_mut(), second_path, Size::new(16.0, 16.0))
            .expect("已登记的资产");

        let mut first_loaded: Vec<AssetLoaded> = Vec::new();
        let mut second_loaded: Vec<AssetLoaded> = Vec::new();
        harness
            .poll_until(600, |app| {
                let mut delivered = AssetImages::poll_all(app, &mut [&mut first, &mut second]);
                first_loaded.append(&mut delivered[0]);
                second_loaded.append(&mut delivered[1]);
                let ready = matches!(first.state(first_path), Some(AssetImageState::Ready(_)))
                    && matches!(second.state(second_path), Some(AssetImageState::Ready(_)));
                ready.then_some(())
            })
            .expect("两张图都必须在 ≈3 s 内交付（任何一方被吞都会超时）");

        assert_eq!(
            first_loaded.len(),
            1,
            "第一个缓存只应交付自己的图：{first_loaded:?}"
        );
        assert_eq!(first_loaded[0].path, first_path);
        assert!(matches!(first_loaded[0].state, AssetImageState::Ready(_)));
        assert_eq!(
            second_loaded.len(),
            1,
            "第二个缓存只应交付自己的图：{second_loaded:?}"
        );
        assert_eq!(second_loaded[0].path, second_path);
        assert!(matches!(second_loaded[0].state, AssetImageState::Ready(_)));

        let empty = AssetImages::poll_all(harness.app_mut(), &mut [&mut first, &mut second]);
        assert!(
            empty.iter().all(Vec::is_empty),
            "交付缓冲必须被一次性取空：{empty:?}"
        );
    }

    #[test]
    fn unknown_assets_are_typed_errors() {
        let mut harness = DecodeApp::new();
        let mut images = AssetImages::new();
        let error = images
            .request(harness.app_mut(), "widgets/nope.png", Size::new(8.0, 8.0))
            .expect_err("未登记必须失败");
        assert_eq!(
            error,
            AssetError::UnknownAsset {
                path: smol_str::SmolStr::new_static("widgets/nope.png")
            }
        );
        assert_eq!(images.state("widgets/nope.png"), None);
    }
}

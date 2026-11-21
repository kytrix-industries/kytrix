//! 应用层资产的注册表与「字节 → 引擎图集」接线。
//!
//! # 与旧实现的关系
//!
//! 旧 `crate::assets::Assets` 是 `rust_embed` + GPUI 的 `AssetSource`：整个 `assets/` 目录被
//! 嵌进二进制，GPUI 在渲染 `svg()` / `img()` 时按路径取字节。引擎没有 `AssetSource` 抽象
//! （设计 §8.5：资源**文件**在应用层，引擎只提供 `ResourceCache`：字节 → 解码 → 图集 tile），
//! 所以本模块做两件事：
//!
//! 1. **注册表**（[`ASSETS`] / [`find`] / [`list`]）：路径 → `&'static [u8]`，覆盖品牌图与
//!    Widget 预览图（`include_bytes!`，构建产物随二进制走）。图标不在这里：它们是
//!    `build.rs` 从 SVG 光栅化出来的 PNG，见 [`crate::kit::icon`]。
//! 2. **异步解码接线**（[`AssetImages`]）：把大图（品牌 / 预览）投给引擎
//!    `ResourceCache::insert_async_at_size`，结果经引擎的每帧 `poll_resources` 进
//!    `App::take_resource_results`，由 [`AssetImages::poll`] 并入状态。UI 线程只做轻量工作，
//!    解码在引擎的 blocking 池里（设计 §8.5 / D-11：解码不在 UI 线程）。
//!
//! # 范围
//!
//! `assets/sounds/news_notification.wav` 不在本批：设计 §9.5 把音效划在引擎范围外，且 `base_ui`
//! 目前没有播放器接线（P2-5 资源收尾时随应用层播放器一起迁移）。
//!
//! # 引用计数 / 生命周期
//!
//! - [`AssetImages`] 的异步插入**没有**窗口归属（引擎 `insert_async*` 不登记 owner）：条目活到
//!   进程结束或应用显式 `App::resources_mut().unload(..)`。这与旧 GPUI 资产的生命周期一致
//!   （资源跟着进程走），也避免每帧重新解码。
//! - [`crate::kit::icon::IconSpec::build`] 走的是**同步**插入 + 窗口归属：小图标（≤96 px）解码
//!   成本可忽略，且图标数量随节点数增长，需要关窗回收。

use std::fmt;

use ahash::HashMap;
use bytes::Bytes;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    math::Size,
    resources::{DecodeCause, DecodeState, PendingResource, ResourceImage},
};

/// 资产类别（决定消费者：品牌 logo vs Widget 占位预览图）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AssetKind {
    /// 品牌 logo（`brands/<Name>/<file>.png`，7 个）。
    Brand,
    /// Widget 预览图（`widgets/<name>.png`，23 个；空状态 / 占位卡片用）。
    WidgetPreview,
}

/// 一个内嵌资产。
#[derive(Clone, Copy, Debug)]
pub struct Asset {
    /// 资产路径（相对 `crates/base_ui/assets`，与旧 `Assets::get` 的键一致）。
    pub path: &'static str,
    /// 类别。
    pub kind: AssetKind,
    /// 文件字节（`include_bytes!`，`'static`）。
    pub bytes: &'static [u8],
}

impl Asset {
    /// 位图文件头里的固有尺寸（逻辑像素；不解码像素，`image` 只读文件头）。
    ///
    /// PNG / JPEG / WebP 之外的字节返回 `None`（注册表里目前只有 PNG）。
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

/// 注册表宏：一份清单同时产出 `ASSETS` 与 `include_bytes!`（路径只写一次）。
macro_rules! asset_table {
    ($($kind:ident => $path:literal),* $(,)?) => {
        /// 全部内嵌资产（品牌在前、预览图在后；顺序稳定，测试锁定数量与唯一性）。
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

/// 按路径查资产（精确匹配；未登记返回 `None`）。
#[must_use]
pub fn find(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

/// 按路径取字节（旧 `Assets::get(path)` 的引擎化替代；未登记返回 `None`）。
#[must_use]
pub fn bytes(path: &str) -> Option<&'static [u8]> {
    find(path).map(|asset| asset.bytes)
}

/// 列出**以 `prefix` 开头**的资产路径（旧 `AssetSource::list(prefix)` 的语义）。
pub fn list(prefix: &str) -> impl Iterator<Item = &'static str> {
    ASSETS
        .iter()
        .filter(move |asset| asset.path.starts_with(prefix))
        .map(|asset| asset.path)
}

/// 一张资产图的解码状态（引擎 `DecodeState` 的应用层镜像）。
#[derive(Clone, Debug, PartialEq)]
pub enum AssetImageState {
    /// 后台解码中（尚未经 `poll` 交付）。
    Pending,
    /// 已进图集，可直接画。
    Ready(ResourceImage),
    /// 解码 / 装箱失败（强类型原因）。
    Failed(DecodeCause),
}

/// 一次交付（[`AssetImages::poll`] 的返回值）：哪张图变成了哪个状态。
#[derive(Clone, Debug, PartialEq)]
pub struct AssetLoaded {
    /// 资产路径。
    pub path: SmolStr,
    /// 新状态（`Ready` / `Failed`）。
    pub state: AssetImageState,
}

/// 资产图的异步解码缓存（应用层持有）。
#[derive(Debug, Default)]
pub struct AssetImages {
    /// 路径 → 状态（`Pending` 在首次 `request` 时写入）。
    entries: HashMap<SmolStr, AssetImageState>,
    /// 在途任务 → 路径（引擎交付时用它把 `PendingResource` 映射回资产）。
    pending: HashMap<PendingResource, SmolStr>,
}

impl AssetImages {
    /// 空缓存。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求一张图（首次调用启动异步解码；已请求过则返回当前状态，不重复投递）。
    ///
    /// `display` 是**物理目标尺寸**（调用方按 `逻辑尺寸 × 窗口 scale` 算好）：引擎
    /// `insert_async_at_size` 按它取变体（不放大、等比 fit，D-10）。同一路径只解码**第一次**
    /// 请求的尺寸 —— 不同尺寸的同名图需要两个 `AssetImages` 实例（本批没有这种消费者）。
    ///
    /// # Errors
    /// [`AssetError::UnknownAsset`]：路径没登记在 [`ASSETS`] 里。
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
        self.entries.insert(path, AssetImageState::Pending);
        Ok(AssetImageState::Pending)
    }

    /// 每帧一次：取走引擎已交付的解码结果并并入缓存（只返回**本次**交付的条目）。
    ///
    /// 交付通道：引擎 `App::poll_resources`（平台泵 / `App::frame` 每轮）把完成项放进
    /// `App::take_resource_results`；这里消费那个缓冲。未交付的项保持 `Pending`。
    pub fn poll(&mut self, app: &mut App) -> Vec<AssetLoaded> {
        let results = app.take_resource_results();
        let mut loaded = Vec::with_capacity(results.len());
        for (pending, state) in results {
            let Some(path) = self.pending.remove(&pending) else {
                continue;
            };
            let state = match state {
                DecodeState::Pending => AssetImageState::Pending,
                DecodeState::Ready(image) => AssetImageState::Ready(image),
                DecodeState::Failed(cause) => AssetImageState::Failed(cause),
            };
            self.entries.insert(path.clone(), state.clone());
            loaded.push(AssetLoaded { path, state });
        }
        loaded
    }

    /// 当前状态（`None` = 从未请求）。
    #[must_use]
    pub fn state(&self, path: &str) -> Option<&AssetImageState> {
        self.entries.get(path)
    }

    /// 已就绪的图（可直接塞进 `Primitive::Image` / `InlineBoxPaint::Image`）。
    #[must_use]
    pub fn image(&self, path: &str) -> Option<ResourceImage> {
        match self.entries.get(path)? {
            AssetImageState::Ready(image) => Some(*image),
            AssetImageState::Pending | AssetImageState::Failed(_) => None,
        }
    }
}

/// 资产请求失败（强类型；目前只有「没登记」这一类，路径本身是可诊断信息）。
#[derive(Clone, Debug, PartialEq)]
pub enum AssetError {
    /// 路径不在 [`ASSETS`] 里。
    UnknownAsset {
        /// 请求的路径。
        path: SmolStr,
    },
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
    use ui_core::{math::Size, resources::ResourceImage};

    use super::{ASSETS, AssetError, AssetImageState, AssetImages, AssetKind, bytes, find, list};
    use crate::kit::test_support::DecodeApp;

    /// 注册表：数量、唯一性、类别分布、路径可反查、字节是 PNG。
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

    /// `list(prefix)` 是前缀语义（旧 `AssetSource::list` 的唯一用途：枚举一个目录）。
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

    /// 固有尺寸从文件头读（不解码像素），品牌图的非正方形长宽比要保真。
    #[test]
    fn native_sizes_come_from_the_file_header() {
        let ethereum = find("brands/Ethereum/colored@72px.png").expect("Ethereum");
        assert_eq!(ethereum.native_size(), Some(Size::new(44.0, 72.0)));
        let solana = find("brands/Solana/colored@72px.png").expect("Solana");
        assert_eq!(solana.native_size(), Some(Size::new(81.0, 72.0)));
        let binance = find("brands/Binance/binance.png").expect("Binance");
        assert_eq!(binance.native_size(), Some(Size::new(128.0, 128.0)));
    }

    /// 异步解码：UI 线程只投递，解码结果由引擎的帧轮询交付，且每项只交付一次。
    #[test]
    fn async_asset_decoding_is_delivered_by_the_engine_poll() {
        let mut harness = DecodeApp::new();
        let mut images = AssetImages::new();
        let path = "brands/Hyperliquid/hyperliquid.png";
        let state = images
            .request(harness.app_mut(), path, Size::new(24.0, 24.0))
            .expect("已登记的资产");
        assert_eq!(state, AssetImageState::Pending, "首次请求是异步的");

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
        // 目标 24×24 的等比 fit：180×180 → 24×24。
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

        // 再 poll 一次不重复交付（引擎的交付缓冲已被取空）。
        assert!(images.poll(harness.app_mut()).is_empty());
        // 重复 request 不重新投递，直接返回已就绪状态。
        assert_eq!(
            images
                .request(harness.app_mut(), path, Size::new(24.0, 24.0))
                .expect("已登记"),
            AssetImageState::Ready(image)
        );
    }

    /// 未登记的路径是强类型错误（不是 panic、也不是空图占位）。
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

//! 图片资源缓存（P13）：字节 → 解码 → 图片图集 tile + 页缓冲。
//!
//! 公开面：[`ResourceId`] / [`ResourceImage`] / [`DecodeCause`] / [`CapacityCause`] /
//! [`ResourceCache`] / [`PendingResource`] / [`DecodeState`] / [`ResourceStats`]；
//! 图集的**字节语义**（线性预乘 → sRGB 编码的 BGRA8）来自 `render/shaders/contract.md` §8，
//! `AtlasTile` / `AtlasKind` 的归属在 `paint::atlas`（DEVIATIONS F-24）。
//!
//! **三条硬规则**：
//!
//! 1. tile 一旦交给应用就**恒有效**，直到引用计数归零（`unload` / `release_window`）——引擎不会
//!    自动淘汰仍在引用的 tile（`Canvas` 是值语义快照，失效 tile 会让旧图元画出别人的图）；
//!    容量不足是强类型 [`DecodeCause::Capacity`]。
//! 2. 不做远程磁盘缓存、不碰 `usvg`：输入只有 `&[u8]` / `Bytes`，字节来源归应用层。
//! 3. 失败是强类型 [`DecodeCause`]，不返回字符串错误、不包装 `image::ImageError`。
//!
//! # 解码不在 UI 线程（R12-15 / D-11）
//!
//! [`ResourceCache::insert_async`] 在 UI 线程只做**轻量**工作：字节长度、内容去重、文件头尺寸
//! （[`image::probe_dimensions`]，O(header)）与**解码前预算准入**；通过后拿到解码并发许可
//! （[`MAX_CONCURRENT_DECODES`]）才把像素解码投到 `tokio::task::spawn_blocking`。
//! [`ResourceCache::poll`] 每帧在 UI 线程把完成的结果装箱进图集。后台线程只拿到
//! `bytes::Bytes`、一个 `oneshot` 发送端与信号量许可，**从不**触碰页缓冲、也从不持有
//! `&mut ResourceCache`。同步 [`ResourceCache::insert`] 保留（小图 / 没有 runtime 的宿主），
//! 它同样先读头 + 准入再解码，结果语义与异步路径完全一致。
//!
//! 源图峰值约束在 `resources::image`：像素数上限 `MAX_SOURCE_PIXELS`（16 M px，取代单纯的
//! 单边 8192）+ 显式 `image::Limits`（`max_alloc` 256 MiB；命中 → [`DecodeCause::OutOfMemory`]）。
//! 准入用 [`image::estimate_peak`]（像素数 × 12 B + 16 MiB 余量，≤ `MAX_DECODE_ALLOC`；12 B/px
//! 含 ICC 转换 dst，D-8）：
//! `图片页 + 存活的源字节 + 在途解码预留 + 本次估算 + 本次源字节 > 预算` ⇒ 立即
//! [`DecodeCause::Capacity`]（[`CapacityCause::ByteBudget`]），**不投递**、不占解码线程。
//! 在途预留从准入一直保持到 [`ResourceCache::poll`] 交付（成功/失败都归还）——完成但未交付的
//! 解码结果（oneshot 里的 BGRA + `PendingJob.source`）仍是常驻内存，提前归还预算会高估可用空间。
//!
//! # 内容去重 / 归属 / 预算（R12-16）
//!
//! - **去重（D-10 两级）**：`ContentKey = (len, ahash(bytes))` 索引 + 原始字节 memcmp（防碰撞）；
//!   每个内容下挂多个**变体**（`(target, ResourceId)`）。同一 `(bytes, target)` 插入 N 次只占
//!   1 块 tile，引用计数 N，命中时**不重新解码**（也不占解码预算）；同一字节的不同变体并存、
//!   各自引用计数，源字节只保留一份（全部变体回收后才释放）。
//! - **归属**：[`ResourceCache::insert_owned`] / [`ResourceCache::insert_owned_at_size`] 把引用
//!   登记到 [`WindowId`] 名下，[`ResourceCache::release_window`] 在该窗口关闭时
//!   （`app::window_set::close_window`）把引用一次性还回去（按 LRU epoch 顺序）。
//! - **预算**：字节预算（图片页 CPU 缓冲 + 去重表保留的源字节 + 在途解码预留）超限 ⇒
//!   [`DecodeCause::Capacity`]（[`CapacityCause::ByteBudget`]）。超预算时**不会**静默淘汰仍在
//!   引用的 tile；空闲条目在 `unload` / `release_window` 里即时回收。`ResourceCache::stats` 是
//!   唯一的可观测面。
//!
//! # 按目标尺寸解码（D-10）
//!
//! `insert_at_size` / `insert_owned_at_size` / `insert_async_at_size` / `insert_for_display` 让应用
//! 按**绘制尺度**取一个变体：`target = ceil(物理目标)` → clamp 到源尺寸（**不放大**）与
//! [`image::MAX_TILE_DIM`]（单块图集上限）→ 解码**全图**后等比 fit 并用 `CatmullRom`（bicubic，
//! 接近 Skia / Qt 的高质量缩放档，比 Lanczos3 少振铃）缩到最终尺寸。默认 `insert*` 仍是
//! 「原始尺寸变体」（必要时按 `MAX_TILE_DIM` 缩小，行为与 D-10 之前完全一致）。
//!
//! - **缓存**：两级（内容 → 变体）。同一内容的不同 target 并存、各自 refcount、`len()` 分别计，
//!   `unload` 一个变体不影响另一个；所有变体归零才删去重条目、释放源字节。`used_pixels` 按
//!   变体最终尺寸累计，页容量 / 预算语义沿用 D-11。
//! - **不省解码峰值**：`image` 0.25.10 没有 decoder 级 scaled decode（`zune-jpeg` 的
//!   `set_max_width` 只是上限校验），准入与并发仍按**源**尺寸估算——target 变小只省图集 tile 与
//!   采样质量，不省解码峰值。
//! - **不做 mipmap**（设计结论）：共享单页图集里给单片 tile 建 mip 链会把相邻 tile 的颜色卷进来；
//!   Chromium 对 `<img>` 也不用 mipmap，而是 `ImageDecodeCache` 按绘制尺度 at-raster 解码——
//!   本 API 就是这一层；放大继续用 GPU 线性采样。
//! - **DPI 2× 资产选择是应用层职责**（同一图片在 2× 设备上用 2× 字节）；引擎不解析 CSS
//!   `image-set`，只保证 `insert_at_size` 的物理目标像素。
//!
//! # 色彩管理（D-8）
//!
//! 带 ICC profile 的资产在解码线程转换到 sRGB（实现与降级策略见 [`image`] 模块文档：sRGB profile
//! 与无 profile 逐字节一致、不可用 profile 按 sRGB 降级、只做 8-bit ICC、不做 gAMA/cHRM）。
//! [`ResourceStats::color_converted`] / [`ResourceStats::color_fallback`] 按**解码次数**累计
//! （内容去重命中不重新解码、也不重复计数），是 HUD / 产品告警面；转换不改变缓存 key
//! （仍是源字节），同一内容的转换结果自然只算一次。
//!
//! # 应用层接线（现状）
//!
//! - `App` 持有进程级 [`ResourceCache`]（`App::new` 里 `ResourceCache::new(runtime)`）；带归属的
//!   插入用 [`ResourceCache::insert_owned`]。
//! - 关窗已接线：`app::window_set::close_window` 在窗口移除后调用
//!   [`ResourceCache::release_window`]（D-7）。
//! - 异步交付仍由宿主驱动：[`ResourceCache::poll`] 需要每帧调一次（帧钩子 / 平台泵）。本分支
//!   尚未把 `poll` 接进 `App` 帧循环，`insert_async` 的结果在接线前只能由调用方自己 poll；
//!   在交付之前，该任务的解码预算预留会一直占着（见上）。

use std::{fmt, num::NonZeroU32, sync::Arc};

use bytes::Bytes;
use smol_str::SmolStr;
use tokio::sync::{
    Semaphore,
    oneshot::{self, error::TryRecvError},
};

use crate::{
    error::EngineError,
    math::Size,
    node::id::{Arena, ArenaKey},
    paint::atlas::{AtlasKind, AtlasTile},
    platform::dispatcher::Dispatcher,
    render::{DirtyRects, ImageAtlasPager},
    resources::{
        cache::{ContentKey, DedupeEntry, Entry, Page, VariantEntry},
        image::{DecodedImage, MAX_TILE_DIM, SourceColor},
    },
    window::WindowId,
};

mod cache;
mod image;

/// 「原始尺寸」变体的变体键（D-10）。
///
/// 与真实目标尺寸（[`image::resolve_target`] 保证 `>= 1`）不冲突：`insert` / `insert_owned` /
/// `insert_async` 的语义仍是「源尺寸变体」（必要时按 `MAX_TILE_DIM` 等比缩小，与 D-10 之前
/// 完全一致），`insert_at_size` 系列才用请求的目标尺寸作键。
const ORIGINAL_TARGET: (u32, u32) = (0, 0);

/// 默认字节预算：图片页（16 MiB）+ 去重表保留的源字节 + 在途解码预留。
///
/// 源字节是去重的代价（memcmp 需要原始缓冲）；图标 / logo 的压缩字节通常只有几十 KiB。
/// 单张合法最大图（16 M px、编码后 ≤ 64 MiB 字节）的准入估算 = 12 B/px × 16 M px = 208 MiB
/// （D-8 起含 ICC 转换 dst）+ 64 MiB 源字节 + 16 MiB 图片页 = 288 MiB，再留 32 MiB 余量 ⇒
/// 320 MiB：预算不会拒绝单张合法图，只拒绝**并发**的大图；「5000 行 × 不同图标」这类工作集也
/// 远达不到预算。ICC 转换不会让预算口径失真（转换 dst 只在解码峰值内、不常驻）。
const DEFAULT_BYTE_BUDGET: u64 = 320 * 1024 * 1024;

/// 同时进行的图片解码数上限（`insert_async` 的 blocking 并发）。
///
/// tokio blocking 池默认上限 512 线程：批量大图不节流会把整机内存打爆（release `panic=abort`
/// ⇒ 分配失败直接死）。2 张 4096² RGBA16 的峰值已接近默认预算，准入会先拒绝第三张；
/// 该上限同时保证「大量小图」不会占满 blocking 池。AGENTS §1.2：不做用户配置层。
const MAX_CONCURRENT_DECODES: usize = 2;

/// 图片资源句柄（8 B，带代际）。
///
/// 字段私有：只能由 [`ResourceCache::insert`] 铸出（`NodeId` 的同类纪律，P02 S-20）。
/// 引用计数归零释放槽位时代际 `+1`，因此旧句柄**永不复活**。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId {
    index: u32,
    generation: NonZeroU32,
}

impl ResourceId {
    /// 槽位下标。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// 代际，恒在 `1..=u32::MAX`（0 永不是合法代际）。
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    /// `(generation as u64) << 32 | index as u64`（结构化日志 / 帧记录的单值标识）。
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

    /// 仅测试用的构造器（不扩大公开面）。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self {
        match NonZeroU32::new(generation) {
            Some(generation) => Self { index, generation },
            None => panic!("ResourceId 的代际不得为 0"),
        }
    }
}

impl fmt::Debug for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ResourceId(r{}.{})", self.index, self.generation)
    }
}

impl ArenaKey for ResourceId {
    fn from_parts(index: u32, generation: u32) -> Self {
        let generation =
            NonZeroU32::new(generation).expect("arena 的代际恒在 1..=u32::MAX（引擎不变量）");
        Self { index, generation }
    }

    fn parts(self) -> (u32, u32) {
        (self.index, self.generation.get())
    }
}

/// 异步插入的句柄（8 B，带代际；语义与 [`ResourceId`] 相同）。
///
/// 字段私有：只能由 [`ResourceCache::insert_async`] 铸出。`poll` 交付某项后槽位被回收，
/// 旧句柄**永不**命中后来的任务。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PendingResource {
    index: u32,
    generation: NonZeroU32,
}

impl PendingResource {
    /// 仅测试用的构造器（不扩大公开面；E7 的图片节点载荷测试用）。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self {
        match NonZeroU32::new(generation) {
            Some(generation) => Self { index, generation },
            None => panic!("PendingResource 的代际不得为 0"),
        }
    }

    /// 槽位下标。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// 代际，恒在 `1..=u32::MAX`（0 永不是合法代际）。
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }
}

impl fmt::Debug for PendingResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PendingResource(p{}.{})", self.index, self.generation)
    }
}

impl ArenaKey for PendingResource {
    fn from_parts(index: u32, generation: u32) -> Self {
        let generation =
            NonZeroU32::new(generation).expect("arena 的代际恒在 1..=u32::MAX（引擎不变量）");
        Self { index, generation }
    }

    fn parts(self) -> (u32, u32) {
        (self.index, self.generation.get())
    }
}

/// 一次成功插入的结果（`Copy`；应用层自己保存，引擎不回查）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceImage {
    /// 该资源的句柄（`unload` 的唯一入口）。
    pub id: ResourceId,
    /// 可直接塞进 `Primitive::Image` / `Canvas::push_image` 的图集矩形。
    pub tile: AtlasTile,
    /// 图集内容区像素宽（缩放后的最终尺寸，`1..=2046`）。
    pub width: u32,
    /// 图集内容区像素高。
    pub height: u32,
}

/// 异步插入的状态（[`ResourceCache::state`] / [`ResourceCache::poll`] 的返回值）。
#[derive(Clone, Debug, PartialEq)]
pub enum DecodeState {
    /// 后台解码仍在进行（或结果还没被 `poll` 交付）。
    Pending,
    /// 已装箱进图集：可以直接画。
    Ready(ResourceImage),
    /// 解码 / 装箱失败（强类型；调用方可按分类画占位图、报错或重试）。
    Failed(DecodeCause),
}

/// [`DecodeCause::Capacity`] 的两种成因（R12-16：页满 / 字节预算不足要能区分）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityCause {
    /// 图片页里放不下该 tile。
    PageFull,
    /// 字节预算（图片页缓冲 + 去重表保留的源字节 + 在途解码预留）放不下该资源。
    ByteBudget {
        /// 当前已用字节。
        used_bytes: u64,
        /// 预算上限。
        limit_bytes: u64,
    },
}

/// 解码 / 装箱失败的业务分类（[`EngineError::ResourceDecode`] 的 payload）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeCause {
    /// 无法识别的格式，或不是 PNG / JPEG / WebP。
    UnsupportedFormat {
        /// 诊断用的稳定格式名（`"gif"` / `"png"` / `"unknown"` …）。
        format: SmolStr,
    },
    /// 字节无法产生有效像素（表单自洽性失败、零尺寸）。
    Corrupt {
        /// 诊断用的稳定格式名。
        format: SmolStr,
        /// `image` 的原始错误文本（只用于日志）。
        detail: SmolStr,
    },
    /// 输入字节长度超过 `MAX_SOURCE_BYTES`（`insert_async` 的轻量校验）。
    SourceTooLarge {
        /// 实际字节数。
        bytes: u64,
        /// 字节上限。
        limit: u64,
    },
    /// 源图尺寸超限：判据是**像素数**（`limit`），单边上限（`dim_limit`）只是纵深防御。
    TooLarge {
        /// 实际宽。
        width: u32,
        /// 实际高。
        height: u32,
        /// 实际像素数（`width * height`）。
        pixels: u64,
        /// 像素数上限（`MAX_SOURCE_PIXELS`）。
        limit: u64,
        /// 单边上限（`MAX_SOURCE_DIM`）。
        dim_limit: u32,
    },
    /// 解码器需要的内存超过 `MAX_DECODE_ALLOC`（`image::Limits` 命中）。
    OutOfMemory {
        /// 解码器分配上限（字节）。
        limit_bytes: u64,
    },
    /// 页满或字节预算不足（`unload` / `release_window` 其它资源后重试）。
    Capacity {
        /// 请求的内容区宽。
        width: u32,
        /// 请求的内容区高。
        height: u32,
        /// 图片页尺寸（纹素）。
        page: (u32, u32),
        /// 两种成因之一。
        cause: CapacityCause,
    },
}

impl fmt::Display for DecodeCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat { format } => write!(
                f,
                "unsupported image format `{format}` (PNG, JPEG and WebP only)"
            ),
            Self::Corrupt { format, detail } => {
                write!(f, "corrupt `{format}` bytes: {detail}")
            }
            Self::SourceTooLarge { bytes, limit } => write!(
                f,
                "image source is {bytes} B, the limit is {limit} B of encoded bytes"
            ),
            Self::TooLarge {
                width,
                height,
                pixels,
                limit,
                dim_limit,
            } => write!(
                f,
                "image is {width}x{height} ({pixels} px), limits are {limit} px total and \
                 {dim_limit} px per side"
            ),
            Self::OutOfMemory { limit_bytes } => write!(
                f,
                "decoding needs more than the {limit_bytes} B decoder memory budget (downscale \
                 the asset before inserting it)"
            ),
            Self::Capacity {
                width,
                height,
                page,
                cause,
            } => match cause {
                CapacityCause::PageFull => write!(
                    f,
                    "a {width}x{height} tile does not fit the {}x{} image atlas: the page is \
                     already full (unload resources you no longer draw to free space)",
                    page.0, page.1
                ),
                CapacityCause::ByteBudget {
                    used_bytes,
                    limit_bytes,
                } => write!(
                    f,
                    "a {width}x{height} tile does not fit the byte budget: {used_bytes} of \
                     {limit_bytes} B already used (unload resources you no longer draw)"
                ),
            },
        }
    }
}

/// 图片资源缓存的诊断快照（HUD / 泄漏排查；与 `TextPassReport` 同一思路）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResourceStats {
    /// 存活资源条目数（引用计数 > 0；等于 [`ResourceCache::len`]）。
    pub entries: usize,
    /// 图片页里已分配的 tile 数（条目与 tile 一一对应：引用计数归零即回收）。
    pub tiles: usize,
    /// 已打开的图片图集页数（D-9 多页；0 = 还没插过图片；上限 `MAX_IMAGE_PAGES`）。
    pub pages: usize,
    /// 存活 tile 的内容区像素总和（缩放后的最终尺寸，不含 padding）。
    pub used_pixels: u64,
    /// CPU 已用字节：图片页缓冲 + 去重表保留的源字节 + 在途解码预留。
    pub used_bytes: u64,
    /// tile 内容区像素 / 全部图片页像素（`0.0..=1.0`；没有图片页时 `0.0`）。
    pub page_occupancy: f32,
    /// 累计 [`DecodeCause::Capacity`] 失败次数（页满 / 预算不足的告警面）。
    pub capacity_errors: u64,
    /// 累计「ICC → sRGB」转换的解码次数（D-8；内容去重命中不重复计）。
    pub color_converted: u64,
    /// 累计「带 ICC 但无法使用、按 sRGB 降级」的解码次数（D-8；损坏 / 不支持 / 超大 / 读取失败；
    /// 内容去重命中不重复计）。产品可据此告警「资产色彩配置有问题」。
    pub color_fallback: u64,
}

/// 图片资源缓存：字节 → 解码 → 图片图集 tile + 页缓冲。
///
/// **不持有 GPU 对象**：页缓冲由 `App::upload_image_page` 交给窗口渲染器（与字形页同一路径）。
/// 进程级一份（单 GPU 设备），由 `App` 持有。
pub struct ResourceCache {
    entries: Arena<Entry, ResourceId>,
    /// 内容去重索引（D-10 两级）：内容 → 原始字节 + 全部存活变体；每个变体一条 [`Entry`]，
    /// 引用计数记在条目上。全部变体回收后整条删除（源字节随之释放）。
    dedupe: ahash::HashMap<ContentKey, DedupeEntry>,
    /// 窗口归属：`insert_owned` 登记，`release_window` 一次性回收。
    owners: ahash::HashMap<WindowId, Vec<ResourceId>>,
    /// 在途的异步解码任务（`insert_async` 铸出 [`PendingResource`]）。
    pending: Arena<PendingJob, PendingResource>,
    /// `pending` 的键表（[`Arena`] 只提供按键访问，没有带键遍历）。
    pending_ids: Vec<PendingResource>,
    /// 图片图集的多页分配器（D-9）：每页 `MAX_ATLAS_PAGE²`，页数上限 `MAX_IMAGE_PAGES`；
    /// 页满按需开新页、已空页按 LRU 回收。**只做分配**（不持有像素）。
    pager: ImageAtlasPager,
    /// 每页的 CPU 像素缓冲（下标 = 页号；`None` = 该页还没写过内容）。
    page_buffers: Vec<Option<Page>>,
    /// 每页最近一次使用的分配代际（页被 LRU 回收复用会换代 ⇒ CPU 页缓冲必须清零重建，
    /// 保证新窗口整页上传时不会把上一轮的旧像素当成有效内容）。
    page_generations: Vec<u32>,
    /// 解码任务投递用的 tokio 句柄；`None` = 无 runtime（`insert_async` 退化为同步解码）。
    runtime: Option<tokio::runtime::Handle>,
    /// 平台事件循环唤醒器：**解码完成时唤醒泵**（D-6）。没有它，空闲时泵阻塞在`GetMessageW`/`poll`，`poll()` 永远不会被调用一次，`insert_async` 的结果与源字节/已解码 BGRA 永不释放。
    dispatcher: Arc<Dispatcher>,
    /// 解码并发信号量：在途 blocking 解码各持有一个许可（上限 [`MAX_CONCURRENT_DECODES`]）。
    decode_semaphore: Arc<Semaphore>,
    /// 在途解码占用的预算（`estimate_peak + 源字节`），[`ResourceCache::poll`] 交付时归还。
    reserved_decode_bytes: u64,
    /// 去重表保留的源字节总和（`stats.used_bytes` 的一部分）。
    source_bytes: u64,
    /// 存活 tile 的内容区像素总和。
    used_pixels: u64,
    /// LRU 时钟：每次成功引用（新建 / 去重命中）`+1`。
    epoch: u64,
    /// 字节预算（图片页 + 去重表源字节 + 在途解码预留）。
    byte_budget: u64,
    /// 累计 `Capacity` 失败次数。
    capacity_errors: u64,
    /// 累计「ICC → sRGB」转换解码次数（D-8 诊断面）。
    color_converted: u64,
    /// 累计「ICC 无法使用 → sRGB 降级」解码次数（D-8 诊断面）。
    color_fallback: u64,
    /// 仅测试：已真正开始解码的 blocking 任务数（证明准入拒绝**没有投递**）。
    #[cfg(test)]
    decode_starts: Arc<std::sync::atomic::AtomicUsize>,
    /// 仅测试：下一次 blocking 解码注入 panic（RD-1：验证 panic 完成路径仍交付+唤醒）。
    #[cfg(test)]
    panic_decode_next: Arc<std::sync::atomic::AtomicBool>,
}

/// RD-1：解码 worker 的交付 guard。
///
/// `image::decode` 对恶意/畸形资产 panic（unwind 构建）时，`oneshot::Sender` 会随闭包展开被
/// drop —— 旧实现此时既不 `send` 也不 `wake()`：纯空闲 UI 不会再 `poll`，预留解码预算与源字节
/// 滞留，后续 `insert_async` 可能被 ByteBudget 永久拒绝。guard 在 `Drop` 里补齐「投递失败结果 +
/// 唤醒泵」，保证「完成/失败必须在有限轮次交付」的契约；正常路径先 `deliver` 结果，`Drop`
/// 只负责 `wake`（幂等电平位）。
struct DecodeGuard {
    sender: Option<oneshot::Sender<Result<DecodedImage, DecodeCause>>>,
    dispatcher: Arc<Dispatcher>,
}

impl DecodeGuard {
    fn new(
        sender: oneshot::Sender<Result<DecodedImage, DecodeCause>>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        Self {
            sender: Some(sender),
            dispatcher,
        }
    }

    fn deliver(&mut self, result: Result<DecodedImage, DecodeCause>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

impl Drop for DecodeGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Err(DecodeCause::Corrupt {
                format: SmolStr::new_static("decode"),
                detail: SmolStr::new_static("decode worker panicked before delivering a result"),
            }));
        }
        // D-6/RD-1：无论正常、失败还是 panic 完成，都必须唤醒泵。
        self.dispatcher.wake();
    }
}

/// 一个在途的异步解码任务。
struct PendingJob {
    /// 后台结果通道；`None` = 没有后台任务（同步路径已给出 `state`）。
    result: Option<oneshot::Receiver<Result<DecodedImage, DecodeCause>>>,
    /// 槽位当前状态（`Pending` = 还在等后台）。
    state: DecodeState,
    /// 源字节（后台完成后的去重 memcmp 用；就绪槽位不保留）。
    source: Bytes,
    /// 处于 `Pending` 时占用的预算（`estimate_peak + 源字节`）；交付时归还。
    reservation: u64,
    /// 该任务的目标变体键（D-10；`(0,0)` = 原始尺寸变体）；交付时据此做 `(bytes, target)` 去重。
    target: (u32, u32),
}

impl ResourceCache {
    /// 新建：异步解码用 `handle` 投递（`runtime` 由 `App` 持有，引擎内不建 runtime、
    /// 也不做 `Handle::current()`——在 runtime 之外会 panic）。
    pub(crate) fn new(handle: tokio::runtime::Handle, dispatcher: Arc<Dispatcher>) -> Self {
        Self::with_runtime(Some(handle), dispatcher)
    }

    /// 无 tokio runtime：`insert_async` **退化为同步解码**并立即给出 `DecodeState::Ready`。
    ///
    /// 只在测试里构造：生产路径的 `ResourceCache` 由 `App` 用 [`ResourceCache::new`] 建（`App`
    /// 永远持有 runtime），不带 runtime 的宿主直接用同步的 [`ResourceCache::insert`]。
    #[cfg(test)]
    pub(crate) fn new_unmanaged() -> Self {
        Self::with_runtime(None, Dispatcher::new())
    }

    fn with_runtime(runtime: Option<tokio::runtime::Handle>, dispatcher: Arc<Dispatcher>) -> Self {
        Self {
            entries: Arena::new(),
            dedupe: ahash::HashMap::default(),
            owners: ahash::HashMap::default(),
            pending: Arena::new(),
            pending_ids: Vec::new(),
            pager: ImageAtlasPager::new(),
            page_buffers: Vec::new(),
            page_generations: Vec::new(),
            runtime,
            dispatcher,
            decode_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_DECODES)),
            reserved_decode_bytes: 0,
            source_bytes: 0,
            used_pixels: 0,
            epoch: 0,
            byte_budget: DEFAULT_BYTE_BUDGET,
            capacity_errors: 0,
            color_converted: 0,
            color_fallback: 0,
            #[cfg(test)]
            decode_starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            panic_decode_next: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// 仅测试：指定页尺寸 / 字节预算 / runtime 的构造器（跳过选档，便于穷尽容量与预算路径）。
    ///
    /// 与旧单页实现一致：构造时**预开第 0 页**（整页标脏 + CPU 页缓冲），既有「页容量/脏矩形」
    /// 测试的初始口径不变；生产路径保持懒分配（首张图片才开页）。
    #[cfg(test)]
    fn for_test(
        page: (u32, u32),
        byte_budget: u64,
        runtime: Option<tokio::runtime::Handle>,
    ) -> Self {
        debug_assert_eq!(page.0, page.1, "图片页必须为正方形");
        let mut cache = Self::with_runtime(runtime, Dispatcher::new());
        cache.byte_budget = byte_budget;
        cache.pager = ImageAtlasPager::with_page_for_test(page.0);
        cache.pager.preopen_page_for_test();
        cache.page_buffers.push(Some(
            Page::new(page.0, page.1).expect("测试页缓冲分配（小页）不应失败"),
        ));
        cache
            .page_generations
            .push(cache.pager.generation(0).unwrap_or(1));
        cache
    }

    /// 仅测试：用指定页尺寸的小页构造。
    #[cfg(test)]
    pub(crate) fn with_page_for_test(page: (u32, u32)) -> Self {
        Self::for_test(page, DEFAULT_BYTE_BUDGET, None)
    }

    /// **图片页固定最大档**（P1-2.7；D-9 起是多页）。
    ///
    /// 旧实现用 `page_for_tile(width, height)` 按**第一张**图片选档并永久锁定：先加载 16×16 图标
    /// ⇒ 页 = 512² ⇒ 之后任何 >510 px 的图必然 `Capacity`，而错误文案还让「unload 后重试」
    /// （页尺寸永不变，重试无效）。加载顺序决定整个进程的图片上限是不可接受的隐式契约。
    ///
    /// D-9 起每页固定最大档（2048²），容纳能力与插入顺序无关；页数按需增长到
    /// `MAX_IMAGE_PAGES`（4 页 ≈ 14.5k 个 32² tile），已空页按 LRU 回收。懒分配语义不变
    /// （没有图片就不占页缓冲/GPU 层）。分配器见 [`ImageAtlasPager`]。
    ///
    /// 解码 `bytes` 并装箱，返回句柄、tile 与像素尺寸（「原始尺寸」变体）。
    ///
    /// 同一内容第二次插入是**去重命中**：不重新解码，返回同一个 [`ResourceImage`]，引用计数 +1。
    /// 需要按绘制尺度取变体时用 [`ResourceCache::insert_at_size`] / [`ResourceCache::insert_for_display`]。
    ///
    /// # Errors
    /// [`EngineError::ResourceDecode`]：`UnsupportedFormat` / `Corrupt` / `SourceTooLarge` /
    /// `TooLarge` / `OutOfMemory` / `Capacity`。
    pub fn insert(&mut self, bytes: &[u8]) -> Result<ResourceImage, EngineError> {
        self.insert_inner(bytes, None)
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    /// 按**物理像素**目标尺寸解码并装箱（D-10），返回该变体的句柄 / tile / 最终尺寸。
    ///
    /// 语义：
    /// - `target` 先 `ceil`（物理像素不能有小数）、防御非有限 / `<= 0`（按 1 像素），再
    ///   `clamp` 到源尺寸与 [`image::MAX_TILE_DIM`] ⇒ **不放大**（Retina 2× 必须由应用提供 2×
    ///   字节）、不会超过单块图集上限；
    /// - 解码**全图**后等比 fit 到 target（`CatmullRom` bicubic），所以 `200×100` + `target 40×40`
    ///   ⇒ `40×20`（不是拉伸成 40×40）；`40×20` + `target 200×200` ⇒ 仍是 `40×20`；
    /// - 变体参与缓存 key：同一份字节的不同 target **并存**、各自引用计数，源字节只保留一份；
    ///   相同 `(bytes, target)`（clamp 后）命中去重、不重新解码；
    /// - **不省解码峰值**：`image` 0.25.10 没有 decoder 级 scaled decode，准入仍按**源**尺寸估算
    ///   （见模块文档「按目标尺寸解码」），target 只省图集 tile 与采样质量。
    ///
    /// # Errors
    /// 同 [`ResourceCache::insert`]。
    pub fn insert_at_size(
        &mut self,
        bytes: &[u8],
        target: Size,
    ) -> Result<ResourceImage, EngineError> {
        self.insert_inner(bytes, Some(target))
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    /// 按**逻辑尺寸 × 设备缩放**取物理像素目标并解码（D-10 的应用便捷入口）。
    ///
    /// `target = ceil(logical * scale)` 物理像素（例如 100×50 逻辑 @2× ⇒ 200×100），其余语义与
    /// [`ResourceCache::insert_at_size`] 相同（不放大 ⇒ 应用负责为 2× 设备提供 2× 资产；
    /// 引擎不解析 CSS `image-set`）。`scale` 为 `<= 0` / 非有限时按 `1.0` 防御。
    ///
    /// # Errors
    /// 同 [`ResourceCache::insert`]。
    pub fn insert_for_display(
        &mut self,
        bytes: &[u8],
        logical: Size,
        scale: f32,
    ) -> Result<ResourceImage, EngineError> {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        self.insert_at_size(
            bytes,
            Size::new(logical.width * scale, logical.height * scale),
        )
    }

    fn insert_inner(
        &mut self,
        bytes: &[u8],
        target: Option<Size>,
    ) -> Result<ResourceImage, DecodeCause> {
        image::check_source_bytes(bytes.len())?;
        // 原始尺寸变体的键与源尺寸无关，去重可以发生在读文件头之前。
        if target.is_none()
            && let Some(image) = self.dedupe_hit(bytes, ORIGINAL_TARGET)
        {
            return Ok(image);
        }
        // 目标尺寸变体的键要 clamp 到源尺寸（不放大 ⇒ 不同请求可能归一成同一变体），
        // 所以必须先读文件头（O(header)）再做去重匹配。
        let (source_width, source_height) = image::probe_dimensions(bytes)?;
        let variant = match target {
            None => ORIGINAL_TARGET,
            Some(target) => image::resolve_target(target, (source_width, source_height)),
        };
        if let Some(image) = self.dedupe_hit(bytes, variant) {
            return Ok(image);
        }
        // D-11：同步路径同样先读头 + 按峰值准入再解码（解码期间没有别的插入能改 `&mut self`，
        // 所以这里的预留只用于复用准入逻辑，解码结束立即归还）。
        let reservation = self.admit_decode(source_width, source_height, bytes.len())?;
        let decoded = match image::decode(bytes, target.map(|_| variant)) {
            Ok(decoded) => decoded,
            Err(cause) => {
                self.release_reservation(reservation);
                return Err(cause);
            }
        };
        self.release_reservation(reservation);
        self.insert_decoded(Bytes::copy_from_slice(bytes), decoded, variant)
    }

    /// 与 [`ResourceCache::insert`] 相同，额外把这次引用登记到 `owner` 名下。
    ///
    /// 关窗时应调用 [`ResourceCache::release_window`] 把名下引用一次性还回去。
    ///
    /// # Errors
    /// 同 [`ResourceCache::insert`]。
    pub fn insert_owned(
        &mut self,
        owner: WindowId,
        bytes: &[u8],
    ) -> Result<ResourceImage, EngineError> {
        let image = self.insert(bytes)?;
        self.owners.entry(owner).or_default().push(image.id);
        Ok(image)
    }

    /// 与 [`ResourceCache::insert_owned`] 相同，但按目标物理尺寸取变体（D-10）。
    ///
    /// # Errors
    /// 同 [`ResourceCache::insert_at_size`]。
    pub fn insert_owned_at_size(
        &mut self,
        owner: WindowId,
        bytes: &[u8],
        target: Size,
    ) -> Result<ResourceImage, EngineError> {
        let image = self.insert_at_size(bytes, target)?;
        self.owners.entry(owner).or_default().push(image.id);
        Ok(image)
    }

    /// 把解码投到 tokio 阻塞池，**立即**返回句柄（UI 线程只做轻量校验 + 头解析 + 预算准入）。
    ///
    /// D-11：超出预算的图在**投递之前**就返回 `Failed(Capacity)`（不占解码线程、不占并发许可）；
    /// 通过准入后在 async 任务里等并发许可（[`MAX_CONCURRENT_DECODES`]），拿到才投 blocking 解码。
    /// 结果通过 [`ResourceCache::poll`] 交付；[`ResourceCache::state`] 可随时查询。
    /// 缓存在测试 / 无 runtime 模式下（`new_unmanaged`）退化为同步解码并立即 `Ready`。
    pub fn insert_async(&mut self, bytes: Bytes) -> PendingResource {
        self.insert_async_inner(bytes, None)
    }

    /// 与 [`ResourceCache::insert_async`] 相同，但交付按目标物理尺寸的变体（D-10）。
    ///
    /// 语义（不放大 / 等比 fit / 变体 key）与 [`ResourceCache::insert_at_size`] 一致；准入与并发
    /// 仍按**源**尺寸的 D-11 口径（decoder 没有 scaled decode，target 不降低解码峰值）。
    /// 去重命中会立即 `Ready`（不投递解码），否则通过 [`ResourceCache::poll`] 交付。
    pub fn insert_async_at_size(&mut self, bytes: Bytes, target: Size) -> PendingResource {
        self.insert_async_inner(bytes, Some(target))
    }

    fn insert_async_inner(&mut self, bytes: Bytes, target: Option<Size>) -> PendingResource {
        if let Err(cause) = image::check_source_bytes(bytes.len()) {
            return self.push_failed(cause);
        }
        // 原始尺寸变体的去重键不需要读文件头。
        if target.is_none()
            && let Some(image) = self.dedupe_hit(&bytes, ORIGINAL_TARGET)
        {
            return self.push_ready(image);
        }
        // 头解析 + 预算准入都必须发生在投递之前：不合格的图不占线程、不占并发许可。
        let (source_width, source_height) = match image::probe_dimensions(&bytes) {
            Ok(dimensions) => dimensions,
            Err(cause) => return self.push_failed(cause),
        };
        let variant = match target {
            None => ORIGINAL_TARGET,
            Some(target) => image::resolve_target(target, (source_width, source_height)),
        };
        if let Some(image) = self.dedupe_hit(&bytes, variant) {
            // 去重命中：不需要解码，立刻可用（也不占解码预算）。
            return self.push_ready(image);
        }
        let reservation = match self.admit_decode(source_width, source_height, bytes.len()) {
            Ok(reservation) => reservation,
            Err(cause) => return self.push_failed(cause),
        };
        let decode_target = target.map(|_| variant);
        let Some(runtime) = self.runtime.clone() else {
            // 无 runtime：退化为同步解码；准入预留立即归还（该路径没有并发解码）。
            self.release_reservation(reservation);
            let state = match image::decode(&bytes, decode_target) {
                Ok(decoded) => self.finish_decoded(bytes, decoded, variant),
                Err(cause) => DecodeState::Failed(cause),
            };
            return self.push_pending(PendingJob {
                result: None,
                state,
                source: Bytes::new(),
                reservation: 0,
                target: variant,
            });
        };
        let (sender, receiver) = oneshot::channel();
        let source = bytes.clone();
        let semaphore = Arc::clone(&self.decode_semaphore);
        let blocking_runtime = runtime.clone();
        let dispatcher = Arc::clone(&self.dispatcher);
        #[cfg(test)]
        let decode_starts = Arc::clone(&self.decode_starts);
        #[cfg(test)]
        let panic_decode_next = Arc::clone(&self.panic_decode_next);
        runtime.spawn(async move {
            // 并发许可在 async 任务里等（**不**占 blocking 线程）；拿到才真正投解码。
            // 信号量只被本 cache 的解码任务共享，永不 close；拿不到是防御性路径，但同样必须
            // 交付失败并唤醒泵（RD-1：否则结果/预留永久滞留）。
            let permit = match semaphore.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    let _ = sender.send(Err(DecodeCause::Corrupt {
                        format: SmolStr::new_static("decode"),
                        detail: SmolStr::new_static(
                            "decode concurrency semaphore closed unexpectedly",
                        ),
                    }));
                    dispatcher.wake();
                    return;
                }
            };
            blocking_runtime.spawn_blocking(move || {
                // 许可活到解码结束（`_permit` 是绑定而不是 `_`，drop 在闭包末尾）。
                let _permit = permit;
                // RD-1：guard 必须在任何可能 panic 的代码之前构造，保证 unwind 路径也投递
                // Failed 并唤醒泵。
                let mut guard = DecodeGuard::new(sender, dispatcher);
                #[cfg(test)]
                {
                    if panic_decode_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        panic!("injected decode worker panic (RD-1 test seam)");
                    }
                    decode_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                let result = image::decode(&source, decode_target);
                guard.deliver(result);
            });
        });
        self.push_pending(PendingJob {
            result: Some(receiver),
            state: DecodeState::Pending,
            source: bytes,
            reservation,
            target: variant,
        })
    }

    /// 每帧在 UI 线程调用一次：把已完成的异步解码装箱进图集。
    ///
    /// 返回**本次**就绪 / 失败的项（每项只交付一次；交付后 `PendingResource` 被回收）。
    /// 后台线程从不触碰图集，装箱（去重 / 分配 / 写页）只在这里发生。交付（成功或失败）时
    /// 归还在途解码预算预留——完成但未交付的 oneshot 结果仍占内存，所以归还得跟着交付走。
    pub fn poll(&mut self) -> Vec<(PendingResource, DecodeState)> {
        // 就地消费键表（不再 clone 整张表 + 事后 filter，热路径零额外分配）。
        let ids = std::mem::take(&mut self.pending_ids);
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if self.pending.get(id).is_none() {
                // 槽位已被回收（防御性：键表与 Arena 不应失配）。
                continue;
            }
            // 1. 同步就绪（去重命中 / 轻量校验失败 / 准入拒绝 / 无 runtime 的退化路径）。
            let ready = self
                .pending
                .get(id)
                .filter(|job| job.result.is_none())
                .map(|job| job.state.clone());
            if let Some(state) = ready {
                self.pending.remove(id);
                out.push((id, state));
                continue;
            }
            // 2. 后台任务：只在这里装箱。
            let Some(completion) = self.take_completion(id) else {
                self.pending_ids.push(id);
                continue;
            };
            let Some(job) = self.pending.remove(id) else {
                continue;
            };
            self.release_reservation(job.reservation);
            let state = match completion {
                Ok(decoded) => self.finish_decoded(job.source, decoded, job.target),
                Err(cause) => DecodeState::Failed(cause),
            };
            out.push((id, state));
        }
        out
    }

    /// 查询异步插入的状态（占位图 / 测试用），**幂等**。
    ///
    /// `poll` 交付某项后句柄被回收，之后的查询返回 [`DecodeState::Pending`]
    /// ——结果已经在 `poll` 的返回值里，句柄不再有意义。
    #[must_use]
    pub fn state(&self, pending: PendingResource) -> DecodeState {
        self.pending
            .get(pending)
            .map_or(DecodeState::Pending, |job| job.state.clone())
    }

    /// 按句柄取**存活**图片的 tile 信息（v2.1 E2 / E7：`InlineBoxPaint::Image` 与
    /// `NodeKind::Image` 的 paint 期解析）。
    ///
    /// 语义：只返回引用计数 > 0 的条目；资源被 `unload` / `release_window` 回收后返回 `None`
    /// （旧句柄不复活 —— `ResourceId` 带代际）。消费者：`paint/pass.rs::emit_inline_box` /
    /// `paint/pass.rs::emit_image`；返回 `None` 时跳过该图元并压 `TextDiagnostic::InlineBoxImageMissing`
    /// （行内盒）或按占位处理（图片节点），都不画占位图、不 panic。
    #[must_use]
    pub(crate) fn image(&self, id: ResourceId) -> Option<ResourceImage> {
        self.entries.get(id).map(|entry| entry.image)
    }

    fn push_pending(&mut self, job: PendingJob) -> PendingResource {
        let id = self.pending.insert_with(|_| job);
        self.pending_ids.push(id);
        id
    }

    /// 立即失败的异步槽位（轻量校验 / 头解析 / 准入拒绝）：不投递解码、不占预算。
    fn push_failed(&mut self, cause: DecodeCause) -> PendingResource {
        self.push_pending(PendingJob {
            result: None,
            state: DecodeState::Failed(cause),
            source: Bytes::new(),
            reservation: 0,
            target: ORIGINAL_TARGET,
        })
    }

    /// 立即就绪的异步槽位（去重命中）：不需要解码，也不占预算。
    fn push_ready(&mut self, image: ResourceImage) -> PendingResource {
        self.push_pending(PendingJob {
            result: None,
            state: DecodeState::Ready(image),
            source: Bytes::new(),
            reservation: 0,
            target: ORIGINAL_TARGET,
        })
    }

    /// 取走一个已完成的后台结果；未完成 / 没有后台任务时 `None`。
    fn take_completion(
        &mut self,
        id: PendingResource,
    ) -> Option<Result<DecodedImage, DecodeCause>> {
        let job = self.pending.get_mut(id)?;
        let received = job.result.as_mut().map(oneshot::Receiver::try_recv)?;
        match received {
            Ok(result) => {
                job.result = None;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Closed) => {
                job.result = None;
                Some(Err(DecodeCause::Corrupt {
                    format: SmolStr::new_static("unknown"),
                    detail: SmolStr::new_static("decode worker dropped without sending a result"),
                }))
            }
        }
    }

    /// 解码结果 → 装箱；失败是 `Failed`（不 panic、不吞掉分类）。
    fn finish_decoded(
        &mut self,
        source: Bytes,
        decoded: DecodedImage,
        target: (u32, u32),
    ) -> DecodeState {
        // 两个在途任务可能是同一 `(bytes, target)`：先交付的那个成功装箱，后到的在这里合并。
        if let Some(image) = self.dedupe_hit(&source, target) {
            return DecodeState::Ready(image);
        }
        match self.insert_decoded(source, decoded, target) {
            Ok(image) => DecodeState::Ready(image),
            Err(cause) => DecodeState::Failed(cause),
        }
    }

    /// 去重命中：同一份字节 + 同一目标尺寸已经有一个 tile ⇒ 引用计数 +1，返回既有结果
    /// （**不重新解码**）。
    fn dedupe_hit(&mut self, bytes: &[u8], target: (u32, u32)) -> Option<ResourceImage> {
        let key = ContentKey::new(bytes);
        let existing = self.dedupe.get(&key)?;
        // 64 bit 哈希碰撞：必须 memcmp 原始字节才能认定「同一内容」。
        if existing.bytes.as_ref() != bytes {
            return None;
        }
        let id = existing
            .variants
            .iter()
            .find(|variant| variant.target == target)?
            .id;
        let epoch = self.next_epoch();
        let Some(entry) = self.entries.get_mut(id) else {
            // 索引里残留了已回收的变体（防御性清理，避免永久占用源字节）。
            self.remove_variant(key, target, id);
            return None;
        };
        entry.refcount = entry.refcount.saturating_add(1);
        entry.last_used = epoch;
        Some(entry.image)
    }

    /// 解码结果 → 图集：装箱写页 → 去重索引（预算准入已在**解码之前**由
    /// [`ResourceCache::admit_decode`] 完成，这里不再重复检查）。
    fn insert_decoded(
        &mut self,
        source: Bytes,
        decoded: DecodedImage,
        target: (u32, u32),
    ) -> Result<ResourceImage, DecodeCause> {
        let color = decoded.color;
        let key = ContentKey::new(&source);
        let image = self.place(decoded, key, target)?;
        // D-8 诊断面：ICC 处理结果按**解码次数**累计（去重命中不重新解码、也不重复计数）。
        match color {
            SourceColor::Srgb => {}
            SourceColor::Converted => {
                self.color_converted = self.color_converted.saturating_add(1);
            }
            SourceColor::AssumedSrgb => {
                self.color_fallback = self.color_fallback.saturating_add(1);
            }
        }
        // 两级索引（D-10）：同内容（memcmp 相等）追加变体；64 bit 哈希碰撞（极少）按旧逻辑
        // 覆盖旧内容并立即释放旧源字节（旧 tile 仍有效，回收时不会再命中去重表）。
        let same_content = self
            .dedupe
            .get(&key)
            .is_some_and(|existing| existing.bytes.as_ref() == source.as_ref());
        if same_content {
            self.dedupe
                .get_mut(&key)
                .expect("checked above")
                .variants
                .push(VariantEntry {
                    target,
                    id: image.id,
                });
            return Ok(image);
        }
        self.source_bytes = self.source_bytes.saturating_add(source.len() as u64);
        if let Some(replaced) = self.dedupe.insert(
            key,
            DedupeEntry {
                bytes: source,
                variants: vec![VariantEntry {
                    target,
                    id: image.id,
                }],
            },
        ) {
            // 哈希碰撞导致同一个 key 覆盖了另一条内容：旧源字节不再保留。
            self.source_bytes = self
                .source_bytes
                .saturating_sub(replaced.bytes.len() as u64);
        }
        Ok(image)
    }

    /// 解码前预算准入（D-11）：`图片页 + 源字节 + 在途预留 + estimate + 本次源字节 ≤ 预算`。
    ///
    /// 通过时把 `estimate + 源字节` 记进 [`ResourceCache::reserved_decode_bytes`] 并返回预留
    /// 字节数（交付时归还）；失败是 [`CapacityCause::ByteBudget`]，**不投递**解码。
    ///
    /// 超预算时**不得**静默淘汰仍在引用的 tile（那会破坏「tile 恒有效」）。空闲条目在
    /// `unload` / `release_window` 里已经即时回收，所以这里没有可淘汰对象，直接报错。
    fn admit_decode(
        &mut self,
        width: u32,
        height: u32,
        source_len: usize,
    ) -> Result<u64, DecodeCause> {
        // RD-2：预算口径必须包含**本次解码可能新开的一页** CPU 缓冲。旧实现只在
        // `page_count()==0` 时预留一页：已开页后 `place` 仍可能再开新页（每页 16 MiB），
        // 最多超出预算 3 页（48 MiB）。这里用保守的最终尺寸估算（源尺寸 clamp 到
        // `MAX_TILE_DIM`；等比 fit 只会更小）判断是否会新开页，并把页字节一起记进预留
        // （交付时归还）——这样 `used_bytes()`（含预留）在任何时刻都 ≤ byte_budget。
        let (page_width, page_height) = self.page_dimensions();
        let page_bytes = u64::from(page_width) * u64::from(page_height) * 4;
        let estimated_content = (width.min(MAX_TILE_DIM), height.min(MAX_TILE_DIM));
        let pending_page_bytes = if self.pager.would_open_new_page(estimated_content) {
            page_bytes
        } else {
            0
        };
        let reservation = image::estimate_peak(width, height)
            .saturating_add(source_len as u64)
            .saturating_add(pending_page_bytes);
        let used = self.used_bytes();
        if used.saturating_add(reservation) <= self.byte_budget {
            self.reserved_decode_bytes = self.reserved_decode_bytes.saturating_add(reservation);
            return Ok(reservation);
        }
        self.capacity_errors = self.capacity_errors.saturating_add(1);
        Err(DecodeCause::Capacity {
            width,
            height,
            page: self.page_dimensions(),
            cause: CapacityCause::ByteBudget {
                used_bytes: used,
                limit_bytes: self.byte_budget,
            },
        })
    }

    /// 归还一笔在途解码预留（交付 / 同步路径结束）。
    fn release_reservation(&mut self, reservation: u64) {
        self.reserved_decode_bytes = self.reserved_decode_bytes.saturating_sub(reservation);
    }

    /// 把解码结果装箱进图片页（预算检查已由调用方完成）。
    ///
    /// 失败只有一种：全部页都放不下（[`CapacityCause::PageFull`]）；失败**不**改变既有条目与
    /// 代际。多页语义（D-9）：页里有空闲空间就复用；页满按需开新页；页数达 `MAX_IMAGE_PAGES`
    /// 后按 LRU 回收**已空**的页（活 tile 永不淘汰）。
    fn place(
        &mut self,
        decoded: DecodedImage,
        key: ContentKey,
        target: (u32, u32),
    ) -> Result<ResourceImage, DecodeCause> {
        let (width, height) = (decoded.width, decoded.height);
        let epoch = self.next_epoch();
        let Some(allocation) = self.pager.allocate((width, height)) else {
            self.capacity_errors = self.capacity_errors.saturating_add(1);
            return Err(DecodeCause::Capacity {
                width,
                height,
                page: self.page_dimensions(),
                cause: CapacityCause::PageFull,
            });
        };
        debug_assert_eq!(allocation.tile.atlas, AtlasKind::Image);
        let page = allocation.tile.page;
        let page_index = usize::from(page);
        // 页缓冲：首次使用 / 页被 LRU 回收复用（分配代际改变）⇒ 重建整页（清零）。
        let generation = self.pager.generation(page).unwrap_or(1);
        let needs_fresh = self.page_generations.get(page_index).copied() != Some(generation)
            || self
                .page_buffers
                .get(page_index)
                .is_none_or(Option::is_none);
        if needs_fresh {
            let (page_width, page_height) = self.page_dimensions();
            if self.page_buffers.len() <= page_index {
                self.page_buffers.resize_with(page_index + 1, || None);
                self.page_generations.resize(page_index + 1, 0);
            }
            // RD-2：页缓冲分配失败映射 `OutOfMemory` 且**回滚**已拿到的图集分配，
            // 不改变既有条目/代际。
            match Page::new(page_width, page_height) {
                Ok(buffer) => {
                    self.page_buffers[page_index] = Some(buffer);
                    self.page_generations[page_index] = generation;
                }
                Err(cause) => {
                    self.pager.deallocate(page, allocation.id);
                    return Err(cause);
                }
            }
        }
        let buffer = self.page_buffers[page_index]
            .as_mut()
            .expect("page buffer is ensured above");
        // padding 复用时可能残留上一块的内容：先清整块（含 padding），再写内容区。
        buffer.clear_rect(allocation.padded);
        buffer.write(allocation.content, &decoded.bgra, width as usize * 4);
        self.pager.mark_written(page, allocation.padded);
        let id = self.entries.insert_with(|id| Entry {
            image: ResourceImage {
                id,
                tile: allocation.tile,
                width,
                height,
            },
            alloc: allocation.id,
            padded: allocation.padded,
            refcount: 1,
            last_used: epoch,
            key,
            target,
        });
        self.used_pixels = self
            .used_pixels
            .saturating_add(u64::from(width) * u64::from(height));
        // RD-2 预算不变量：页缓冲在 `place` 里才真正计入 `used_bytes()`；准入阶段已按
        // `would_open_new_page` 把这一页的字节算进预留，因此此刻（本任务预留已归还）的
        // `used_bytes()`（含其它在途预留）必须仍 ≤ 预算。
        debug_assert!(
            self.used_bytes() <= self.byte_budget,
            "RD-2 预算不变量被破坏：used={} budget={}",
            self.used_bytes(),
            self.byte_budget
        );
        Ok(self
            .entries
            .get(id)
            .expect("just inserted entry must exist")
            .image)
    }

    /// 释放**一次**引用：引用计数 -1，归零才回收 tile 与内容索引。
    ///
    /// **契约**：调用方必须保证不再有 `Image` 图元引用该 tile（通常是先卸载/重建对应节点）。
    /// 误用时该区域被清零（表现为透明），若区域随后被新资源复用则旧图元会采样到新图。
    /// 返回 `false` = 陈旧句柄 / 引用已归零（幂等）。
    pub fn unload(&mut self, id: ResourceId) -> bool {
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        entry.refcount = entry.refcount.saturating_sub(1);
        let reclaimed = entry.refcount == 0;
        if reclaimed {
            self.reclaim(id);
        }
        true
    }

    /// 关窗回收：把 `owner` 名下的引用全部还回去，并删掉归属记录。
    ///
    /// 回收顺序按 `last_used`（LRU）：最久没被引用的先还。**不影响**其它窗口对同一内容的引用。
    /// 窗口不存在时不会走到这里（`app::window_set::close_window` 的重复调用提前返回），
    /// 因此重复关窗是幂等的。
    pub fn release_window(&mut self, owner: WindowId) {
        let Some(mut ids) = self.owners.remove(&owner) else {
            return;
        };
        ids.sort_by_key(|id| self.entries.get(*id).map_or(0, |entry| entry.last_used));
        for id in ids {
            self.unload(id);
        }
    }

    /// 诊断快照（HUD / 泄漏排查）。
    #[must_use]
    pub fn stats(&self) -> ResourceStats {
        let (page_width, page_height) = self.page_dimensions();
        let page_pixels = u64::from(page_width) * u64::from(page_height);
        let pages = self.pager.page_count();
        let entries = self.entries.len();
        ResourceStats {
            entries,
            tiles: entries,
            pages,
            used_pixels: self.used_pixels,
            used_bytes: self.used_bytes(),
            page_occupancy: if page_pixels == 0 || pages == 0 {
                0.0
            } else {
                self.used_pixels as f32 / (page_pixels * pages as u64) as f32
            },
            capacity_errors: self.capacity_errors,
            color_converted: self.color_converted,
            color_fallback: self.color_fallback,
        }
    }

    /// 存活变体数（引用计数 > 0 的条目数；同一变体被多次去重命中仍是 1，同一内容的不同 target
    /// 变体分别计，D-10）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空（快照路径的上传开关）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 已打开的图片图集页数（D-9 多页；0 = 还没插过图片；上限 `MAX_IMAGE_PAGES`）。
    pub(crate) fn page_count(&self) -> usize {
        self.pager.page_count()
    }

    /// 图片图集单页的边长（纹素；所有页相同，生产恒 `MAX_ATLAS_PAGE`）。
    pub(crate) fn page_extent(&self) -> u32 {
        self.page_dimensions().0
    }

    /// 某页的当前内容（`page.0 * page.1 * 4` 字节，BGRA8 预乘 sRGB）；页不存在时 `None`。
    pub(crate) fn page_pixels_at(&self, page: u16) -> Option<((u32, u32), &[u8])> {
        let buffer = self.page_buffers.get(usize::from(page))?.as_ref()?;
        Some((buffer.dimensions(), buffer.bytes()))
    }

    /// 第 0 页的当前内容（单页测试路径的便捷入口；多页上传由 `App` 按页遍历）。
    #[cfg(test)]
    pub(crate) fn page_pixels(&self) -> Option<((u32, u32), &[u8])> {
        self.page_pixels_at(0)
    }

    /// 取走某页自上次上传以来被写脏的**矩形**；页不存在时 `None`。
    pub(crate) fn take_dirty_rects_at(&mut self, page: u16) -> Option<DirtyRects> {
        self.pager.take_dirty_rects(page)
    }

    /// 第 0 页的待上传矩形（单页测试路径的便捷入口；没有页时是空集）。
    #[cfg(test)]
    pub(crate) fn take_dirty_rects(&mut self) -> DirtyRects {
        self.take_dirty_rects_at(0).unwrap_or_default()
    }

    /// 回收一条引用归零的资源：退还 tile、清零页内区域（含 padding）、从去重表删除该变体
    /// （最后一个变体删除时才释放源字节，D-10）。
    fn reclaim(&mut self, id: ResourceId) {
        let Some(entry) = self.entries.remove(id) else {
            return;
        };
        self.remove_variant(entry.key, entry.target, id);
        self.used_pixels = self
            .used_pixels
            .saturating_sub(u64::from(entry.image.width) * u64::from(entry.image.height));
        let page = entry.image.tile.page;
        if let Some(buffer) = self
            .page_buffers
            .get_mut(usize::from(page))
            .and_then(Option::as_mut)
        {
            buffer.clear_rect(entry.padded);
        }
        self.pager.deallocate(page, entry.alloc);
        self.pager.mark_written(page, entry.padded);
    }

    /// 从去重表的 `key` 内容里删掉 `(target, id)` 这一个变体；变体全部归零才删条目、释放源字节
    /// （D-10 两级缓存：同一内容的所有变体共享一份源字节）。
    fn remove_variant(&mut self, key: ContentKey, target: (u32, u32), id: ResourceId) {
        let Some(dedupe) = self.dedupe.get_mut(&key) else {
            return;
        };
        dedupe
            .variants
            .retain(|variant| variant.id != id || variant.target != target);
        let empty = dedupe.variants.is_empty();
        if empty && let Some(removed) = self.dedupe.remove(&key) {
            self.source_bytes = self.source_bytes.saturating_sub(removed.bytes.len() as u64);
        }
    }

    fn next_epoch(&mut self) -> u64 {
        self.epoch = self.epoch.saturating_add(1);
        self.epoch
    }

    fn page_dimensions(&self) -> (u32, u32) {
        let size = self.pager.page_size();
        (size, size)
    }

    fn used_bytes(&self) -> u64 {
        let (width, height) = self.page_dimensions();
        let page_bytes = u64::from(width) * u64::from(height) * 4;
        page_bytes * self.pager.page_count() as u64 + self.source_bytes + self.reserved_decode_bytes
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use bytes::Bytes;

    use super::{
        CapacityCause, DEFAULT_BYTE_BUDGET, DecodeCause, DecodeState, MAX_CONCURRENT_DECODES,
        PendingResource, ResourceCache, ResourceId, ResourceImage, ResourceStats,
    };
    use crate::{
        error::EngineError,
        math::{PhysicalRect, Size},
        paint::atlas::{AtlasKind, MAX_IMAGE_PAGES},
        resources::image::{MAX_SOURCE_BYTES, MAX_SOURCE_DIM, MAX_SOURCE_PIXELS},
        window::WindowId,
    };

    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        encoded(width, height, rgba, image::ImageFormat::Png)
    }

    /// 指定尺寸 / 颜色 / 格式的编码字节（JPEG 只能写 RGB，`write_to` 会做转换）。
    fn encoded(width: u32, height: u32, rgba: [u8; 4], format: image::ImageFormat) -> Vec<u8> {
        let pixels: Vec<u8> = std::iter::repeat_n(rgba, (width * height) as usize)
            .flatten()
            .collect();
        rgba_png(width, height, &pixels, format)
    }

    /// 指定 RGBA8 像素的编码字节（保持 `format` 与 `encoded` 一致）。
    fn rgba_png(width: u32, height: u32, pixels: &[u8], format: image::ImageFormat) -> Vec<u8> {
        let image = image::RgbaImage::from_raw(width, height, pixels.to_vec()).expect("raw");
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, format)
            .expect("encode");
        out.into_inner()
    }

    /// 四象限纯色图（左上红、右上绿、左下蓝、右下白）：用于断言缩放后的方向与等比 fit。
    fn quadrant_png(width: u32, height: u32) -> Vec<u8> {
        let half_width = width / 2;
        let half_height = height / 2;
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                let color = match (x < half_width, y < half_height) {
                    (true, true) => [255u8, 0, 0, 255],
                    (false, true) => [0, 255, 0, 255],
                    (true, false) => [0, 0, 255, 255],
                    (false, false) => [255, 255, 255, 255],
                };
                pixels.extend_from_slice(&color);
            }
        }
        rgba_png(width, height, &pixels, image::ImageFormat::Png)
    }

    fn cache() -> ResourceCache {
        // 16×16 的页：只放得下一块 14×14 的内容（+2 padding）。
        ResourceCache::with_page_for_test((16, 16))
    }

    fn inserted(cache: &mut ResourceCache, rgba: [u8; 4]) -> ResourceImage {
        cache.insert(&png(4, 4, rgba)).expect("insert 4x4 png")
    }

    fn insert_error(cache: &mut ResourceCache, bytes: &[u8]) -> EngineError {
        cache.insert(bytes).expect_err("must fail")
    }

    fn decode_cause(error: EngineError) -> DecodeCause {
        match error {
            EngineError::ResourceDecode { cause } => cause,
            other => panic!("expected ResourceDecode, got {other:?}"),
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    /// 轮询直到 `pending` 交付（上限 200 × 1 ms）；用于异步状态机测试。
    fn poll_until(cache: &mut ResourceCache, pending: PendingResource) -> DecodeState {
        for _ in 0..200 {
            for (handle, state) in cache.poll() {
                if handle == pending {
                    return state;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("async decode did not finish within 200 ms");
    }

    #[test]
    fn resource_id_is_8_bytes() {
        assert_eq!(size_of::<ResourceId>(), 8);
        assert_eq!(size_of::<Option<ResourceId>>(), 8);
    }

    #[test]
    fn pending_resource_is_8_bytes() {
        assert_eq!(size_of::<PendingResource>(), 8);
        assert_eq!(size_of::<Option<PendingResource>>(), 8);
    }

    #[test]
    fn resource_id_to_bits_layout() {
        let id = ResourceId::for_test(3, 7);
        assert_eq!(id.to_bits(), (7u64 << 32) | 3);
        assert_eq!(id.index(), 3);
        assert_eq!(id.generation(), 7);
        assert_eq!(format!("{id:?}"), "ResourceId(r3.7)");
    }

    /// R12-16：同一份字节插入两次是**去重**，不是两块 tile。
    ///
    /// 旧测试（`insert_returns_a_tile_per_call`）断言「每次插入一条独立 tile」——那正是 R12-16
    /// 指出的缺陷：行情表 100 行同一个 logo 会占 100 块 tile 直到 `Capacity`。现在断言：
    /// 同一内容同一个 `ResourceImage`、`len() == 1`、引用计数 2；`unload` 一次不回收、两次才回收。
    #[test]
    fn insert_dedupes_identical_bytes() {
        let mut cache = cache();
        let bytes = png(4, 4, [1, 2, 3, 255]);
        let first = cache.insert(&bytes).expect("first insert");
        let second = cache
            .insert(&bytes)
            .expect("second insert hits the content index");

        assert_eq!(first, second, "同一份字节必须复用同一 tile / 同一句柄");
        assert_eq!(cache.len(), 1, "去重后只有一条条目");
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(
            cache.entries.get(first.id).expect("entry").refcount,
            2,
            "两次插入 = 引用计数 2"
        );

        assert!(cache.unload(first.id));
        assert_eq!(cache.len(), 1, "引用计数未归零，不得回收 tile");
        assert!(cache.unload(second.id));
        assert_eq!(cache.len(), 0, "引用计数归零才回收");
        assert!(cache.dedupe.is_empty(), "回收后内容索引必须同步清掉");
        assert_eq!(cache.stats().used_pixels, 0);
        assert_eq!(cache.stats().used_bytes, 16 * 16 * 4, "只剩图片页本身");
    }

    #[test]
    fn insert_png_happy_path() {
        let mut cache = ResourceCache::new_unmanaged();
        let image = inserted(&mut cache, [10, 20, 30, 128]);

        assert_eq!((image.width, image.height), (4, 4));
        assert_eq!(image.tile.atlas, AtlasKind::Image);
        assert_eq!(image.tile.generation, 1);
        assert!(image.tile.u0 < image.tile.u1 && image.tile.v0 < image.tile.v1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn insert_rejects_unsupported_and_unknown_formats() {
        let mut cache = cache();
        assert_eq!(
            decode_cause(insert_error(
                &mut cache,
                b"GIF89a\x01\x00\x01\x00\x00\x00\x00"
            )),
            DecodeCause::UnsupportedFormat {
                format: smol_str::SmolStr::new_static("gif")
            }
        );
        assert_eq!(
            decode_cause(insert_error(&mut cache, &[0u8; 8])),
            DecodeCause::UnsupportedFormat {
                format: smol_str::SmolStr::new_static("unknown")
            }
        );
        assert_eq!(cache.len(), 0, "失败的插入不得留下条目");
    }

    #[test]
    fn insert_rejects_corrupt_png() {
        let mut cache = cache();
        let png = png(4, 4, [1, 2, 3, 255]);
        let error = decode_cause(insert_error(&mut cache, &png[..png.len() / 2]));
        assert!(
            matches!(&error, DecodeCause::Corrupt { format, .. } if format == "png"),
            "{error:?}"
        );
        assert_eq!(cache.len(), 0);
    }

    /// R12-15：尺寸判据走像素数 / 单边上限，分类是 `TooLarge`（不是 `Corrupt`）。
    #[test]
    fn insert_reports_source_size_limits() {
        let mut cache = cache();
        let wide = png(MAX_SOURCE_DIM + 1, 1, [1, 2, 3, 255]);
        assert_eq!(
            decode_cause(insert_error(&mut cache, &wide)),
            DecodeCause::TooLarge {
                width: MAX_SOURCE_DIM + 1,
                height: 1,
                pixels: u64::from(MAX_SOURCE_DIM) + 1,
                limit: MAX_SOURCE_PIXELS,
                dim_limit: MAX_SOURCE_DIM,
            }
        );
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.stats().capacity_errors, 0, "尺寸失败不是容量失败");
    }

    /// P1-2.7 回归：图片页的容纳能力**不得**依赖插入顺序。
    ///
    /// 旧实现按第一张图选档并永久锁定：先插 16×16 图标 ⇒ 页 = 512² ⇒ 之后 600×600 必然
    /// `Capacity`，且 `unload` 后重试也无效（页尺寸永不变）。
    #[test]
    fn page_capacity_is_independent_of_insertion_order() {
        let mut cache = ResourceCache::new_unmanaged();
        // 先一张小图（旧实现会把页钉在 512²）。
        let tiny = inserted(&mut cache, [1, 2, 3, 255]);
        assert_eq!((tiny.width, tiny.height), (4, 4));
        // 再一张远超 512 的图：必须能放下。
        let big = cache.insert(&png(600, 600, [9, 9, 9, 255])).expect(
            "600x600 must fit after a tiny image (the page is not locked by the first insert)",
        );
        assert_eq!((big.width, big.height), (600, 600));
        // 两张都还活着，且指向同一页。
        assert_eq!(tiny.tile.atlas, big.tile.atlas);
        assert_eq!(cache.len(), 2);
    }

    /// 逐张插入不同颜色的 6×6 PNG，直到容量失败；返回成功插入的 image 列表与失败原因。
    ///
    /// 页尺寸固定 8×8：6×6 内容 + 2 padding 恰好占满一页，因此这个循环会按页增长地把
    /// `MAX_IMAGE_PAGES` 页全部用满（不依赖 etagere 的具体装箱取整细节）。
    fn fill_until_capacity(cache: &mut ResourceCache) -> (Vec<ResourceImage>, u64, DecodeCause) {
        let mut images = Vec::new();
        let mut source_bytes = 0u64;
        let mut value = 1u8;
        loop {
            let bytes = png(
                6,
                6,
                [value, value.wrapping_mul(3), value.wrapping_mul(5), 255],
            );
            match cache.insert(&bytes) {
                Ok(image) => {
                    images.push(image);
                    source_bytes += bytes.len() as u64;
                    value = value.wrapping_add(1);
                }
                Err(error) => return (images, source_bytes, decode_cause(error)),
            }
        }
    }

    #[test]
    fn insert_reports_capacity_with_sizes() {
        // D-9 多页：必须把 `MAX_IMAGE_PAGES` 页全部填满才报 PageFull。
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let (images, _source, error) = fill_until_capacity(&mut cache);
        assert!(!images.is_empty());
        assert_eq!(
            cache.stats().pages,
            usize::from(MAX_IMAGE_PAGES),
            "容量失败前必须真的把每一页都用上"
        );
        assert_eq!(
            error,
            DecodeCause::Capacity {
                width: 6,
                height: 6,
                page: (8, 8),
                cause: CapacityCause::PageFull,
            }
        );
        assert_eq!(cache.len(), images.len(), "容量失败不改变既有条目");
        assert_eq!(cache.stats().capacity_errors, 1, "容量失败要计数（诊断面）");
    }

    #[test]
    fn capacity_failure_does_not_bump_generation() {
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let (images, _source, _error) = fill_until_capacity(&mut cache);
        let image = images[0];
        let generation = image.tile.generation;

        assert_eq!(
            cache
                .entries
                .get(image.id)
                .expect("entry survives")
                .image
                .tile,
            image.tile
        );
        assert!(!image.tile.is_stale(generation), "容量失败不换代（F-140）");
    }

    /// D-9：多页图集——单页放满后自动开新页，页号从 0 开始逐个使用。
    #[test]
    fn image_atlas_grows_to_multiple_pages() {
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let (images, source, _error) = fill_until_capacity(&mut cache);
        let mut pages: Vec<u16> = images.iter().map(|image| image.tile.page).collect();
        pages.sort_unstable();
        pages.dedup();
        assert_eq!(
            pages,
            (0..MAX_IMAGE_PAGES).collect::<Vec<_>>(),
            "每一页都必须被使用（页号 0..MAX_IMAGE_PAGES）"
        );
        let stats = cache.stats();
        assert_eq!(stats.pages, usize::from(MAX_IMAGE_PAGES));
        assert_eq!(stats.tiles, images.len());
        // 页缓冲按页常驻：4 × 8² × 4 B + 全部存活变体的源字节。
        assert_eq!(stats.used_bytes, 4 * 8 * 8 * 4 + source);
        let expected = (images.len() as f32 * 36.0) / (f32::from(MAX_IMAGE_PAGES) * 64.0);
        assert!(
            (stats.page_occupancy - expected).abs() < 1e-6,
            "占用率按全部页数计：{} vs {expected}",
            stats.page_occupancy
        );
    }

    /// X4：一帧 ≥9 块图片的脏矩形必须折叠为 **bbox**（仍 < 整页），不得退化为整页 16 MiB
    /// 上传/拷贝尖峰。
    #[test]
    fn nine_image_tiles_upload_a_bounding_box_not_the_whole_page() {
        let mut cache = ResourceCache::with_page_for_test((2048, 2048));
        let _ = cache.take_dirty_rects(); // 首帧整页已上传
        // 9 块 34×34 的 tile 分散在页面上（直接写脏，避免装箱把它们拼成相邻矩形而提前合并）。
        for index in 0..9i32 {
            let x = (index % 3) * 600;
            let y = (index / 3) * 600;
            cache
                .pager
                .mark_written(0, PhysicalRect::new(x, y, x + 34, y + 34));
        }
        let rects = cache.take_dirty_rects();
        assert!(!rects.is_full(), "X4：9 块图片不得折叠成整页哨兵");
        let page_bytes = 2048u64 * 2048 * 4;
        let uploaded = rects.byte_count(2048, 4) as u64;
        assert!(
            uploaded < page_bytes,
            "X4：上传字节 {uploaded} 必须小于整页 {page_bytes}"
        );
        assert_eq!(rects.chunks(2048, 4).count(), 1, "折叠结果是单个 bbox 矩形");
    }

    /// D-9/RD-3：活 tile 永不淘汰；只有**已空**的页按 LRU 回收复用（页号保持稳定），
    /// 且必须精确按「最久未用」顺序（8² 页 + 6² 内容 = 每页恰好一块，顺序可观测）。
    #[test]
    fn empty_pages_are_recycled_lru() {
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let (images, _source, _error) = fill_until_capacity(&mut cache);
        assert_eq!(cache.stats().pages, usize::from(MAX_IMAGE_PAGES));
        let page1 = images
            .iter()
            .find(|image| image.tile.page == 1)
            .copied()
            .expect("page 1 must be used");
        let page2 = images
            .iter()
            .find(|image| image.tile.page == 2)
            .copied()
            .expect("page 2 must be used");
        // 先释放页 1（更久未用），再释放页 2（更新）⇒ LRU 必须先用页 1、再用页 2。
        assert!(cache.unload(page1.id));
        assert!(cache.unload(page2.id));
        let recycled_first = cache
            .insert(&png(6, 6, [9, 9, 9, 255]))
            .expect("已空页必须被回收复用");
        assert_eq!(
            recycled_first.tile.page, 1,
            "必须精确回收最久未用的空页（页 1），而不是任意空页"
        );
        assert_ne!(
            recycled_first.tile.generation, page1.tile.generation,
            "回收复用必须换代"
        );
        let recycled_second = cache
            .insert(&png(6, 6, [10, 10, 10, 255]))
            .expect("第二个空页");
        assert_eq!(recycled_second.tile.page, 2, "其次回收较近变空的页 2");
        assert_eq!(
            cache.stats().pages,
            usize::from(MAX_IMAGE_PAGES),
            "回收复用不得增加页数"
        );
        // 使用中保护：页 0/3 仍有活 tile；4 页均已无空闲（每页一块）⇒ 再插入必须失败，
        // 且失败不得回收仍被引用的页。
        let error = decode_cause(insert_error(&mut cache, &png(6, 6, [11, 11, 11, 255])));
        assert!(
            matches!(
                &error,
                DecodeCause::Capacity {
                    cause: CapacityCause::PageFull,
                    ..
                }
            ),
            "页全满时必须 PageFull：{error:?}"
        );
        assert_eq!(cache.len(), 4, "活 tile 不得被淘汰");
    }

    /// D-9：每页有独立的 CPU 页缓冲与脏矩形（多页上传的数据面）。
    #[test]
    fn page_buffers_and_dirty_rects_are_per_page() {
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let first = cache.insert(&png(6, 6, [11, 22, 33, 255])).expect("first");
        let second = cache.insert(&png(6, 6, [44, 55, 66, 255])).expect("second");
        assert_eq!((first.tile.page, second.tile.page), (0, 1));
        // 清掉两页的待上传矩形后，只写第 3 页：只有第 3 页脏。
        let _ = cache.take_dirty_rects_at(0);
        let _ = cache.take_dirty_rects_at(1);
        let third = cache.insert(&png(6, 6, [77, 88, 99, 255])).expect("third");
        assert_eq!(third.tile.page, 2);
        assert!(
            cache
                .take_dirty_rects_at(2)
                .is_some_and(|rects| !rects.is_empty())
        );
        assert!(
            cache
                .take_dirty_rects_at(0)
                .is_some_and(|rects| rects.is_empty())
        );
        // 页缓冲按页可寻址，内容在各自页里。
        let (dims, pixels) = cache.page_pixels_at(1).expect("page 1 buffer");
        assert_eq!(dims, (8, 8));
        let origin_x = (second.tile.u0 * 8.0).round() as usize;
        let origin_y = (second.tile.v0 * 8.0).round() as usize;
        let offset = (origin_y * 8 + origin_x) * 4;
        assert_eq!(&pixels[offset..offset + 4], &[66, 55, 44, 255]);
    }

    #[test]
    fn unload_is_idempotent_and_stale_handles_stay_dead() {
        let mut cache = cache();
        let image = inserted(&mut cache, [1, 2, 3, 255]);

        assert!(cache.unload(image.id));
        assert!(!cache.unload(image.id), "重复卸载是 no-op");
        assert_eq!(cache.len(), 0);

        // 槽位复用后代际不同 ⇒ 旧句柄仍然无效。
        let replacement = inserted(&mut cache, [4, 5, 6, 255]);
        assert_eq!(replacement.id.index(), image.id.index(), "槽位被复用");
        assert_ne!(replacement.id.generation(), image.id.generation());
        assert!(!cache.unload(image.id));
    }

    #[test]
    fn unload_clears_the_page_region_and_marks_rects_dirty() {
        let mut cache = cache();
        let image = inserted(&mut cache, [200, 100, 50, 255]);
        let region = image_region(&cache, image);
        let _ = cache.take_dirty_rects(); // 首帧已上传整页
        let before: Vec<u8> = cache.page_pixels().expect("page").1.to_vec();
        assert!(before.iter().any(|byte| *byte != 0), "插入必须写进页");

        assert!(cache.unload(image.id));
        let after = cache.page_pixels().expect("page").1;
        assert!(after.iter().all(|byte| *byte == 0), "卸载后区域必须清零");
        let rects = cache.take_dirty_rects();
        assert!(!rects.is_empty(), "清零区必须上报脏矩形");
        // AOI 契约（R06-3）：清零的是**整块 padded 矩形**，x 区间同样要上报。
        assert_eq!(rects.rects(), &[region]);
    }

    #[test]
    fn first_dirty_rects_cover_the_whole_page() {
        let mut cache = cache();
        let rects = cache.take_dirty_rects();
        assert_eq!(rects.rects().len(), 1);
        assert_eq!(
            rects.rects()[0],
            PhysicalRect::new(0, 0, 16, 16),
            "首次上传必须是整页（GPU 侧内容未定义）"
        );
        assert!(rects.covers_page(16));
        assert_eq!(rects.byte_count(16, 4), 16 * 16 * 4);
        assert!(cache.take_dirty_rects().is_empty(), "take 会清空");
    }

    #[test]
    fn insert_marks_only_the_allocated_rect_dirty() {
        let mut cache = cache();
        let _ = cache.take_dirty_rects();
        let image = inserted(&mut cache, [1, 2, 3, 255]);

        let rects = cache.take_dirty_rects();
        assert_eq!(rects.rects().len(), 1, "{:?}", rects.rects());
        // AOI 契约（R06-3）：脏矩形 = padded 块（含 1 纹素 padding）的**完整矩形** ——
        // 旧实现只上报 y 区间，单个 4×4 图片也要上传整段页宽的行。
        let region = image_region(&cache, image);
        assert_eq!(rects.rects()[0], region);
        assert_eq!(
            rects.byte_count(16, 4),
            region.area() as usize * 4,
            "只上传该矩形的字节，而不是 region.height() 行 × 页宽"
        );
        assert!(
            rects.byte_count(16, 4) <= region.area() as usize * 4,
            "AOI 字节数不得超过 padded 块的面积 × texel"
        );
    }

    /// 一条资源在图集页上的整块矩形（padded 块：内容 + 1 纹素 padding）。
    fn image_region(cache: &ResourceCache, image: ResourceImage) -> PhysicalRect {
        cache.entries.get(image.id).expect("entry").padded
    }

    #[test]
    fn page_pixels_hold_premultiplied_bgra() {
        let mut cache = cache();
        let image = inserted(&mut cache, [128, 128, 128, 255]);
        let (dimensions, pixels) = cache.page_pixels().expect("page");
        assert_eq!(dimensions, (16, 16));

        // 内容区原点 = tile 的 UV 左边界 × 页宽（不引入内部字段就能定位）。
        let origin_x = (image.tile.u0 * 16.0).round() as usize;
        let origin_y = (image.tile.v0 * 16.0).round() as usize;
        let offset = origin_y * 16 * 4 + origin_x * 4;
        assert_eq!(&pixels[offset..offset + 4], &[128, 128, 128, 255]);
    }

    #[test]
    fn failed_insert_leaves_the_page_untouched() {
        let mut cache = cache();
        let page_before: Vec<u8> = cache.page_pixels().expect("page").1.to_vec();
        let _ = insert_error(&mut cache, b"GIF89a\x01\x00\x01\x00\x00\x00\x00");

        assert_eq!(cache.page_pixels().expect("page").1, page_before.as_slice());
        assert_eq!(cache.len(), 0);
    }

    /// R12-15 的状态机：`insert_async` 立即 `Pending`，`poll` 之后 `Ready` 且已装箱。
    #[test]
    fn insert_async_reaches_ready_after_poll() {
        let runtime = runtime();
        let mut cache = ResourceCache::for_test(
            (64, 64),
            super::DEFAULT_BYTE_BUDGET,
            Some(runtime.handle().clone()),
        );
        let pending = cache.insert_async(Bytes::from(png(8, 8, [1, 2, 3, 255])));
        assert_eq!(
            cache.state(pending),
            DecodeState::Pending,
            "UI 线程不做解码"
        );
        assert_eq!(cache.len(), 0, "解码完成前不占图集");

        let state = poll_until(&mut cache, pending);
        let DecodeState::Ready(image) = state else {
            panic!("expected Ready, got {state:?}");
        };
        assert_eq!((image.width, image.height), (8, 8));
        assert_eq!(image.tile.atlas, AtlasKind::Image);
        assert!(image.tile.u0 < image.tile.u1 && image.tile.v0 < image.tile.v1);
        assert_eq!(cache.len(), 1);
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.tiles, 1);
        assert_eq!(stats.used_pixels, 64);
        assert_eq!(stats.capacity_errors, 0);
    }

    #[test]
    fn insert_async_reports_corrupt_png_as_failed() {
        let runtime = runtime();
        let mut cache = ResourceCache::for_test(
            (64, 64),
            super::DEFAULT_BYTE_BUDGET,
            Some(runtime.handle().clone()),
        );
        let png = png(4, 4, [1, 2, 3, 255]);
        let pending = cache.insert_async(Bytes::from(png[..png.len() / 2].to_vec()));
        let state = poll_until(&mut cache, pending);

        assert!(
            matches!(&state, DecodeState::Failed(DecodeCause::Corrupt { format, .. }) if format == "png"),
            "{state:?}"
        );
        assert_eq!(cache.len(), 0, "失败的异步解码不得留下条目");
        assert_eq!(cache.stats().entries, 0);
    }

    /// 无 runtime 的退化契约：`insert_async` 同步解码并立即 `Ready`。
    #[test]
    fn unmanaged_insert_async_is_ready_immediately() {
        let mut cache = cache();
        let pending = cache.insert_async(Bytes::from(png(4, 4, [9, 9, 9, 255])));

        let DecodeState::Ready(image) = cache.state(pending) else {
            panic!("unmanaged 模式必须同步解码并立即 Ready");
        };
        assert_eq!((image.width, image.height), (4, 4));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.poll().len(), 1, "poll 交付一次");
        assert!(cache.poll().is_empty(), "交付后不再重复");
    }

    /// 去重命中不需要解码：`insert_async` 立刻 `Ready`（即使没有 runtime 也一样）。
    #[test]
    fn insert_async_hits_the_content_index_without_decoding() {
        let mut cache = cache();
        let bytes = png(4, 4, [7, 8, 9, 255]);
        let sync = cache.insert(&bytes).expect("sync insert");
        let pending = cache.insert_async(Bytes::from(bytes));

        assert_eq!(
            cache.state(pending),
            DecodeState::Ready(sync),
            "去重命中不需要任何解码"
        );
        assert_eq!(cache.poll().len(), 1);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.entries.get(sync.id).expect("entry").refcount, 2);
    }

    #[test]
    fn insert_async_rejects_oversize_source_bytes() {
        let mut cache = cache();
        let pending = cache.insert_async(Bytes::from(vec![0u8; MAX_SOURCE_BYTES + 1]));

        assert!(
            matches!(
                cache.state(pending),
                DecodeState::Failed(DecodeCause::SourceTooLarge { .. })
            ),
            "轻量校验必须在 UI 线程直接拒绝"
        );
        assert_eq!(cache.len(), 0);
    }

    /// 交付之后句柄被回收；槽位复用后代际不同，旧句柄不会命中新任务。
    #[test]
    fn delivered_pending_handles_are_reclaimed() {
        let mut cache = cache();
        let first = cache.insert_async(Bytes::from(png(4, 4, [1, 2, 3, 255])));
        let delivered = cache.poll();
        let [(handle, DecodeState::Ready(_image))] = delivered.as_slice() else {
            panic!("expected exactly one Ready item, got {delivered:?}");
        };
        assert_eq!(*handle, first);
        assert_eq!(
            cache.state(first),
            DecodeState::Pending,
            "交付后句柄被回收（结果已在 poll 的返回值里）"
        );

        let second = cache.insert_async(Bytes::from(png(4, 4, [4, 5, 6, 255])));
        assert_eq!(second.index(), first.index(), "槽位复用");
        assert_ne!(second.generation(), first.generation(), "代际递增");
        assert_ne!(second, first);
    }

    /// R12-16 归属：两个窗口共享同一 tile，关一个不影响另一个。
    #[test]
    fn release_window_reclaims_only_that_windows_resources() {
        let mut cache = ResourceCache::with_page_for_test((64, 64));
        let logo = png(8, 8, [1, 2, 3, 255]);
        let avatar = png(8, 8, [4, 5, 6, 255]);
        let left = WindowId::for_test(1);
        let right = WindowId::for_test(2);
        let left_logo = cache.insert_owned(left, &logo).expect("left logo");
        let right_logo = cache.insert_owned(right, &logo).expect("right logo");
        let right_avatar = cache.insert_owned(right, &avatar).expect("right avatar");

        assert_eq!(left_logo, right_logo, "两个窗口共享同一 tile（去重）");
        assert_eq!(cache.len(), 2, "两种内容 ⇒ 两条条目");

        cache.release_window(left);
        assert_eq!(cache.len(), 2, "右窗口仍持有 logo，不得回收");
        assert_eq!(
            cache.insert(&logo).expect("logo still alive"),
            right_logo,
            "去重命中仍指向同一 tile（左窗口的释放不影响右窗口）"
        );
        assert!(cache.unload(right_logo.id), "手上有一次额外引用");

        cache.release_window(right);
        assert_eq!(cache.len(), 0, "右窗口名下的引用全部还清");
        assert_eq!(cache.stats().used_pixels, 0);
        assert!(cache.dedupe.is_empty());
        assert!(
            !cache.unload(right_avatar.id),
            "已回收的句柄是陈旧的（幂等）"
        );

        // 已经释放过的窗口再释放一次是 no-op。
        cache.release_window(right);
        assert_eq!(cache.len(), 0);
    }

    /// R12-16 预算 + D-11 准入：超预算的插入是 `Capacity`（`ByteBudget`），`unload` 之后能再插。
    ///
    /// D-11 起预算口径是「图片页 + 源字节 + 在途解码预留」，准入在**解码前**按
    /// `estimate_peak(4,4) + 源字节` 预留，所以这里的预算按「页 + 一张的预留 + 第二张源字节 − 1」
    /// 构造：第一张放得下，第二张在解码前就被拒。
    #[test]
    fn byte_budget_reports_capacity_and_recovers_after_unload() {
        let first_bytes = png(4, 4, [1, 2, 3, 255]);
        let second_bytes = png(4, 4, [4, 5, 6, 255]);
        let page_bytes = 16 * 16 * 4;
        let budget = page_bytes
            + super::image::estimate_peak(4, 4)
            + first_bytes.len() as u64
            + second_bytes.len() as u64
            - 1;
        let mut cache = ResourceCache::for_test((16, 16), budget, None);

        let first = cache.insert(&first_bytes).expect("第一张必须在预算内");
        let error = decode_cause(insert_error(&mut cache, &second_bytes));
        assert!(
            matches!(
                &error,
                DecodeCause::Capacity {
                    cause: CapacityCause::ByteBudget { .. },
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(cache.len(), 1, "预算失败不改变既有条目");
        assert_eq!(cache.stats().capacity_errors, 1);
        assert_eq!(cache.reserved_decode_bytes, 0, "准入失败不得留下预留");

        assert!(cache.unload(first.id));
        let second = cache.insert(&second_bytes).expect("unload 之后预算有空间");
        assert_ne!(second.id, first.id);
        assert_eq!(cache.stats().capacity_errors, 1, "成功插入不再累计失败");
    }

    /// RD-2：**新开页**也必须走解码前预算准入（旧实现只在页数=0 时预留一页，已开页后再开
    /// 新页可让 `used_bytes` 超出预算最多 3 页 = 48 MiB）。这里用 16² 页 + 14² 内容（每页恰好
    /// 一块）构造：第 2 页允许、第 3 页在**解码前**被 ByteBudget 拒绝，且不变量 `used ≤ budget` 恒成立。
    #[test]
    fn new_page_allocation_is_admitted_by_the_byte_budget() {
        let page_bytes = 16u64 * 16 * 4;
        let first_bytes = png(14, 14, [10, 20, 30, 255]);
        let second_bytes = png(14, 14, [40, 50, 60, 255]);
        let third_bytes = png(14, 14, [70, 80, 90, 255]);
        let source = |bytes: &[u8]| bytes.len() as u64;
        // 预算 = 2 页 + 一次峰值估算 + 一份源字节 + 半页余量：第 2 张允许，第 3 张拒绝。
        let budget = page_bytes * 2
            + super::image::estimate_peak(14, 14)
            + source(&first_bytes)
            + page_bytes / 2;
        let mut cache = ResourceCache::for_test((16, 16), budget, None);
        assert_eq!(cache.stats().pages, 1, "测试构造预开第 0 页");

        let first = cache.insert(&first_bytes).expect("第一张（复用预开页）");
        assert_eq!(first.tile.page, 0);
        assert!(cache.stats().used_bytes <= budget);

        let second = cache.insert(&second_bytes).expect("第二张必须被准入到新页");
        assert_eq!(second.tile.page, 1, "第二张必须落在新开的第 2 页");
        assert_eq!(cache.stats().pages, 2);
        assert!(
            cache.stats().used_bytes <= budget,
            "开新页后 used_bytes 仍必须 ≤ 预算：{} > {budget}",
            cache.stats().used_bytes
        );

        let error = decode_cause(insert_error(&mut cache, &third_bytes));
        assert!(
            matches!(
                &error,
                DecodeCause::Capacity {
                    cause: CapacityCause::ByteBudget { .. },
                    ..
                }
            ),
            "第 3 页必须由字节预算在解码前拒绝：{error:?}"
        );
        assert_eq!(cache.stats().pages, 2, "被拒的插入不得开页");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.reserved_decode_bytes, 0, "准入失败不得留下预留");
        assert!(cache.stats().used_bytes <= budget);
    }

    #[test]
    fn stats_track_page_usage_and_entries() {
        let mut cache = cache();
        assert_eq!(
            cache.stats(),
            ResourceStats {
                entries: 0,
                tiles: 0,
                pages: 1,
                used_pixels: 0,
                used_bytes: 16 * 16 * 4,
                page_occupancy: 0.0,
                capacity_errors: 0,
                color_converted: 0,
                color_fallback: 0,
            }
        );

        let image = inserted(&mut cache, [1, 2, 3, 255]);
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.tiles, 1);
        assert_eq!(stats.used_pixels, 16);
        assert!(stats.used_bytes > 16 * 16 * 4, "源字节也要计入预算");
        assert!((stats.page_occupancy - 16.0 / 256.0).abs() < 1e-6);

        assert!(cache.unload(image.id));
        let stats = cache.stats();
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.used_pixels, 0);
        assert_eq!(stats.used_bytes, 16 * 16 * 4, "图片页本身不释放");
        assert_eq!(stats.page_occupancy, 0.0);
    }

    /// 页内某条资源内容区的第一个像素（由 tile 的归一化 UV 反推页内偏移）。
    fn content_pixel(cache: &ResourceCache, image: ResourceImage) -> [u8; 4] {
        content_pixel_at(cache, image, 0, 0)
    }

    /// 页内某条资源内容区 `(x, y)` 处的 BGRA 像素（D-10 的方向 / 缩放断言用）。
    fn content_pixel_at(cache: &ResourceCache, image: ResourceImage, x: u32, y: u32) -> [u8; 4] {
        assert!(x < image.width && y < image.height, "越界采样 {x},{y}");
        let ((page_width, page_height), pixels) = cache.page_pixels().expect("page");
        let origin_x = (image.tile.u0 * page_width as f32).round() as usize;
        let origin_y = (image.tile.v0 * page_height as f32).round() as usize;
        let offset = ((origin_y + y as usize) * page_width as usize + origin_x + x as usize) * 4;
        pixels[offset..offset + 4].try_into().expect("4 bytes")
    }

    /// 页内某条资源内容区的全部 BGRA 字节（逐变体比较像素用）。
    fn tile_pixels(cache: &ResourceCache, image: ResourceImage) -> Vec<u8> {
        let ((page_width, page_height), pixels) = cache.page_pixels().expect("page");
        let origin_x = (image.tile.u0 * page_width as f32).round() as usize;
        let origin_y = (image.tile.v0 * page_height as f32).round() as usize;
        let row_bytes = image.width as usize * 4;
        let mut out = Vec::with_capacity(row_bytes * image.height as usize);
        for y in 0..image.height as usize {
            let start = ((origin_y + y) * page_width as usize + origin_x) * 4;
            out.extend_from_slice(&pixels[start..start + row_bytes]);
        }
        out
    }

    /// D-8：坏 ICC ⇒ 解码成功（不让插入失败）、像素 == 无 profile 路径、
    /// `stats.color_fallback` 计数。
    #[test]
    fn decode_invalid_icc_falls_back_to_srgb_and_counts() {
        use super::image::test_png;

        let mut cache = ResourceCache::with_page_for_test((64, 64));
        let pixels = [12u8, 34, 56, 128];
        let plain = test_png(1, 1, &pixels, None, None);
        let broken = test_png(1, 1, &pixels, Some(b"broken icc"), None);
        assert_eq!(cache.stats().color_fallback, 0);

        let plain_image = cache.insert(&plain).expect("无 profile 的资产");
        let broken_image = cache.insert(&broken).expect("坏 ICC 不得使插入失败");
        let stats = cache.stats();
        assert_eq!(stats.color_fallback, 1, "坏 ICC 必须计入降级计数");
        assert_eq!(stats.color_converted, 0);
        assert_eq!(stats.entries, 2);
        assert_eq!(
            content_pixel(&cache, broken_image),
            content_pixel(&cache, plain_image),
            "降级路径的像素必须等于无 profile 路径"
        );
    }

    /// D-8 诊断面：ICC 转换按**解码次数**累计（去重命中不重新解码、不重复计数）。
    #[test]
    fn icc_conversion_is_counted_once_per_decode() {
        use super::image::test_png;

        let mut cache = ResourceCache::with_page_for_test((64, 64));
        let p3 = moxcms::ColorProfile::new_display_p3()
            .encode()
            .expect("encode display p3");
        // sRGB 红的 P3 编码（解码后应变为 sRGB 红）。
        let tagged = test_png(1, 1, &[234, 51, 35, 255], Some(&p3), None);
        cache.insert(&tagged).expect("第一次插入解码并转换");
        cache.insert(&tagged).expect("同字节再去重命中，不重新解码");
        let stats = cache.stats();
        assert_eq!(
            stats.color_converted, 1,
            "去重命中不重新解码，也就不得重复计数"
        );
        assert_eq!(stats.color_fallback, 0);
        assert_eq!(stats.entries, 1);
    }

    /// D-10：按目标尺寸解码——等比 fit、不拉伸（200×100 + target 40×40 ⇒ 40×20），
    /// 缩小的像素方向与原图一致（四象限着色）。
    #[test]
    fn insert_at_size_downscales_and_keeps_aspect() {
        let mut cache = ResourceCache::with_page_for_test((128, 128));
        let bytes = quadrant_png(200, 100);
        let image = cache
            .insert_at_size(&bytes, Size::new(40.0, 40.0))
            .expect("按目标尺寸插入 200x100 -> 40x40");

        assert_eq!(
            (image.width, image.height),
            (40, 20),
            "必须等比 fit 到 40×20，而不是拉伸成 40×40"
        );
        assert_eq!(
            cache.stats().used_pixels,
            40 * 20,
            "used_pixels 按目标尺寸累计"
        );
        assert_eq!(image.tile.atlas, AtlasKind::Image);

        // 四象限中心：缩放后方向不得翻转/交换（CatmullRom 只在象限边界过渡）。
        let red = content_pixel_at(&cache, image, 10, 5);
        let green = content_pixel_at(&cache, image, 30, 5);
        let blue = content_pixel_at(&cache, image, 10, 15);
        let white = content_pixel_at(&cache, image, 30, 15);
        assert!(
            red[2] > 200 && red[0] < 40 && red[1] < 40,
            "左上必须是红（BGRA）：{red:?}"
        );
        assert!(
            green[1] > 200 && green[0] < 40 && green[2] < 40,
            "右上必须是绿（BGRA）：{green:?}"
        );
        assert!(
            blue[0] > 200 && blue[1] < 40 && blue[2] < 40,
            "左下必须是蓝（BGRA）：{blue:?}"
        );
        assert!(
            white.iter().all(|channel| *channel > 200),
            "右下必须是白：{white:?}"
        );
    }

    /// D-10：目标大于源尺寸时**不放大**（Retina 2× 的 1× 资产必须由应用提供 2× 字节）。
    #[test]
    fn insert_at_size_never_upscales() {
        let mut cache = ResourceCache::with_page_for_test((128, 128));
        let bytes = png(40, 20, [90, 120, 150, 255]);
        let image = cache
            .insert_at_size(&bytes, Size::new(200.0, 200.0))
            .expect("目标大于源");

        assert_eq!(
            (image.width, image.height),
            (40, 20),
            "不得把 40×20 放大成 200×200（应用负责提供 2× 资产）"
        );
        // 不放大 ⇒ 像素与「原始尺寸变体」逐字节一致。
        let original = cache.insert(&bytes).expect("原始尺寸变体");
        assert_eq!((original.width, original.height), (40, 20));
        assert_eq!(
            tile_pixels(&cache, image),
            tile_pixels(&cache, original),
            "不放大时像素必须与原始尺寸路径一致"
        );
        assert_eq!(cache.len(), 2, "按目标尺寸与原始尺寸是两个变体");
    }

    /// D-10 两级缓存：同一内容的不同 target 变体并存、各自引用计数；源字节只保留一份。
    #[test]
    fn variants_of_the_same_content_coexist_and_refcount_independently() {
        let mut cache = ResourceCache::with_page_for_test((128, 128));
        let bytes = png(200, 100, [30, 60, 90, 255]);
        let large = cache
            .insert_at_size(&bytes, Size::new(80.0, 80.0))
            .expect("大缩略图");
        let small = cache
            .insert_at_size(&bytes, Size::new(20.0, 20.0))
            .expect("小缩略图");

        assert_ne!(large.id, small.id, "不同 target 是不同的变体");
        assert_eq!((large.width, large.height), (80, 40));
        assert_eq!((small.width, small.height), (20, 10));
        assert_eq!(cache.len(), 2, "两个变体各占一条 Entry");
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().used_pixels, 80 * 40 + 20 * 10);
        // 源字节只保留一份（两级缓存：内容一份 + 变体 N 条）。
        assert_eq!(cache.source_bytes, bytes.len() as u64, "源字节只存一份");
        assert_eq!(
            cache.stats().used_bytes,
            128 * 128 * 4 + bytes.len() as u64,
            "预算 = 图片页 + 一份源字节"
        );
        assert_eq!(cache.dedupe.len(), 1, "一条内容");
        assert_eq!(
            cache
                .dedupe
                .values()
                .next()
                .expect("dedupe entry")
                .variants
                .len(),
            2,
            "该内容下两个变体"
        );

        // unload 一个大变体：小变体不受影响，源字节仍被小变体持有。
        assert!(cache.unload(large.id));
        assert_eq!(cache.len(), 1, "另一个变体仍存活");
        assert_eq!(cache.stats().used_pixels, 20 * 10);
        assert_eq!(cache.source_bytes, bytes.len() as u64, "还有变体存活");
        let small_again = cache
            .insert_at_size(&bytes, Size::new(20.0, 20.0))
            .expect("小变体仍存活，命中去重");
        assert_eq!(small_again, small, "同 (bytes, target) 去重命中");
        assert_eq!(cache.len(), 1);
        assert_eq!(
            content_pixel(&cache, small_again),
            content_pixel(&cache, small),
            "去重命中后 tile 内容仍是原来的变体"
        );
        // 引用计数 2：再 unload 一次才回收。
        assert!(cache.unload(small_again.id));
        assert_eq!(cache.len(), 1);
        assert!(cache.unload(small.id));
        assert_eq!(cache.len(), 0, "最后一个变体回收");
        assert!(cache.dedupe.is_empty(), "全部变体归零才删去重条目");
        assert_eq!(cache.source_bytes, 0, "全部变体归零才释放源字节");
        assert_eq!(cache.stats().used_bytes, 128 * 128 * 4, "只剩图片页常驻");
    }

    /// D-10：回收最后一个变体时同步释放源字节与 tile（预算口径回到页常驻）。
    #[test]
    fn unload_releases_the_only_variant_and_source_bytes() {
        let mut cache = ResourceCache::with_page_for_test((64, 64));
        let bytes = png(200, 100, [10, 20, 30, 255]);
        let image = cache
            .insert_at_size(&bytes, Size::new(40.0, 40.0))
            .expect("按目标尺寸插入");
        assert_eq!(cache.source_bytes, bytes.len() as u64);
        let page_bytes = 64 * 64 * 4;
        assert!(cache.stats().used_bytes > page_bytes, "源字节在预算里");

        assert!(cache.unload(image.id));
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().used_pixels, 0);
        assert_eq!(cache.stats().used_bytes, page_bytes, "回到页常驻");
        assert_eq!(cache.source_bytes, 0);
        assert!(cache.dedupe.is_empty());
        // 陈旧句柄幂等（与原始尺寸变体同一语义）。
        assert!(!cache.unload(image.id));
    }

    /// D-10：`insert_for_display` 把逻辑尺寸 × 设备缩放取整为物理像素，且不放大。
    #[test]
    fn insert_for_display_rounds_to_physical_pixels_and_does_not_upscale() {
        let mut cache = ResourceCache::with_page_for_test((256, 256));
        let wide = png(200, 100, [1, 2, 3, 255]);
        // 100×50 逻辑 @1.5× ⇒ 150×75 物理像素（等比 fit 后正好 150×75）。
        let hd = cache
            .insert_for_display(&wide, Size::new(100.0, 50.0), 1.5)
            .expect("1.5x");
        assert_eq!((hd.width, hd.height), (150, 75));

        // 非整数逻辑尺寸必须向上取整到物理像素：30.2 × 30.2 @2× ⇒ ceil(60.4) = 61。
        let square = png(100, 100, [7, 8, 9, 255]);
        let rounded = cache
            .insert_for_display(&square, Size::new(30.2, 30.2), 2.0)
            .expect("ceil 物理像素");
        assert_eq!((rounded.width, rounded.height), (61, 61));

        // 2× 设备 + 1× 资产：不放大（应用层负责 2× 资产选择）。
        let small = png(40, 20, [4, 5, 6, 255]);
        let at_2x = cache
            .insert_for_display(&small, Size::new(40.0, 20.0), 2.0)
            .expect("2x");
        assert_eq!((at_2x.width, at_2x.height), (40, 20));

        // scale <= 0 / 非有限按 1.0 防御（不是 0 尺寸 tile、不 panic）。
        for scale in [0.0f32, -2.0, f32::NAN, f32::INFINITY] {
            let defensive = cache
                .insert_for_display(&small, Size::new(40.0, 20.0), scale)
                .expect("防御 scale");
            assert_eq!(
                (defensive.width, defensive.height),
                (40, 20),
                "scale {scale} 必须按 1.0 处理"
            );
        }
    }

    /// D-10：异步路径同样交付请求的变体（D-11 的准入 / 并发 / 预留语义不变）。
    #[test]
    fn async_insert_at_size_delivers_the_requested_variant() {
        let runtime = runtime();
        let mut cache = ResourceCache::for_test(
            (128, 128),
            DEFAULT_BYTE_BUDGET,
            Some(runtime.handle().clone()),
        );
        let bytes = png(200, 100, [12, 34, 56, 255]);
        let pending = cache.insert_async_at_size(Bytes::from(bytes), Size::new(40.0, 40.0));
        assert_eq!(
            cache.state(pending),
            DecodeState::Pending,
            "UI 线程不做解码"
        );

        let state = poll_until(&mut cache, pending);
        let DecodeState::Ready(image) = state else {
            panic!("expected Ready, got {state:?}");
        };
        assert_eq!((image.width, image.height), (40, 20), "异步路径也等比 fit");
        assert_eq!(cache.stats().used_pixels, 40 * 20);
        assert_eq!(cache.len(), 1);

        // 同一 bytes + 同一 target 的异步插入命中去重、立即 Ready。
        let again = cache.insert_async_at_size(
            Bytes::from(png(200, 100, [12, 34, 56, 255])),
            Size::new(40.0, 40.0),
        );
        assert_eq!(
            cache.state(again),
            DecodeState::Ready(image),
            "异步去重命中立即 Ready、不重新解码"
        );
        assert_eq!(cache.poll().len(), 1);
        assert_eq!(cache.len(), 1);
    }

    /// D-11：预算准入在**投递之前**拒绝，不占解码线程、不留预留。
    #[test]
    fn async_decode_admission_rejects_before_spawning() {
        let runtime = runtime();
        let bytes = png(200, 200, [1, 2, 3, 255]);
        let page_bytes = 16 * 16 * 4;
        // 预算连「页 + 估算」都不够：准入必然拒绝。
        let budget = page_bytes + super::image::estimate_peak(200, 200) - 1;
        let mut cache = ResourceCache::for_test((16, 16), budget, Some(runtime.handle().clone()));

        let pending = cache.insert_async(Bytes::from(bytes));
        let state = cache.state(pending);
        assert!(
            matches!(
                &state,
                DecodeState::Failed(DecodeCause::Capacity {
                    cause: CapacityCause::ByteBudget { .. },
                    ..
                })
            ),
            "超预算必须在 UI 线程立即 Failed，而不是投递后才发现：{state:?}"
        );
        assert_eq!(cache.stats().capacity_errors, 1);
        assert_eq!(cache.reserved_decode_bytes, 0, "拒绝的图不得占用预算预留");
        assert_eq!(
            cache
                .decode_starts
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "准入拒绝不得投递解码任务"
        );
        assert_eq!(cache.len(), 0, "失败不得留下图集条目");

        // Failed 仍然通过 poll 交付一次（与轻量校验失败同一语义）。
        let delivered = cache.poll();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, pending);
        assert_eq!(delivered[0].1, state);
        assert!(cache.poll().is_empty(), "交付后不得重复");
    }

    /// RD-1：worker panic（unwind 构建）也必须投递 `Failed(Corrupt)` 并唤醒泵、归还预留——
    /// 旧实现只在 `sender.send` 之后 `wake`，panic 时两者都不发生，纯空闲 UI 会永久滞留。
    #[test]
    fn decode_worker_panic_delivers_failure_and_wakes() {
        let runtime = runtime();
        let mut cache = ResourceCache::for_test(
            (16, 16),
            DEFAULT_BYTE_BUDGET,
            Some(runtime.handle().clone()),
        );
        let _ = cache.take_dirty_rects();
        let _ = cache.dispatcher.take_frame_wake();
        cache
            .panic_decode_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let pending = cache.insert_async(Bytes::from(png(4, 4, [7, 8, 9, 255])));
        assert_eq!(cache.state(pending), DecodeState::Pending);

        let state = poll_until(&mut cache, pending);
        assert!(
            matches!(state, DecodeState::Failed(DecodeCause::Corrupt { .. })),
            "worker panic 必须交付 Failed(Corrupt)：{state:?}"
        );
        assert_eq!(cache.reserved_decode_bytes, 0, "panic 完成必须归还预留");
        assert!(
            cache.dispatcher.take_frame_wake(),
            "panic 完成必须唤醒平台泵（否则结果/预留滞留）"
        );
        assert!(cache.poll().is_empty(), "失败结果只交付一次");
    }

    /// D-11：在途解码预留从准入保持到 poll 交付；交付后归还，下一张才能插入。
    #[test]
    fn async_decode_reservations_are_returned_after_completion() {
        let runtime = runtime();
        let first_bytes = png(4, 4, [1, 2, 3, 255]);
        let second_bytes = png(4, 4, [4, 5, 6, 255]);
        let page_bytes = 16 * 16 * 4;
        // 准入预留 = 峰值估算 + 源字节；预算只够**一张**的预留。
        let first_reservation = super::image::estimate_peak(4, 4) + first_bytes.len() as u64;
        let second_reservation = super::image::estimate_peak(4, 4) + second_bytes.len() as u64;
        let budget = page_bytes + first_reservation + second_reservation - 1;
        let mut cache = ResourceCache::for_test((16, 16), budget, Some(runtime.handle().clone()));

        let first = cache.insert_async(Bytes::from(first_bytes));
        assert_eq!(cache.state(first), DecodeState::Pending);
        assert_eq!(
            cache.reserved_decode_bytes, first_reservation,
            "第一张必须占着在途预留（含源字节）"
        );

        // 第二张在准入被第一张的预留挡住：立即 Failed，不投递。
        let second = cache.insert_async(Bytes::from(second_bytes.clone()));
        assert!(
            matches!(
                cache.state(second),
                DecodeState::Failed(DecodeCause::Capacity {
                    cause: CapacityCause::ByteBudget { .. },
                    ..
                })
            ),
            "第二张必须在准入阶段被拒"
        );

        let state = poll_until(&mut cache, first);
        assert!(matches!(state, DecodeState::Ready(_)), "{state:?}");
        assert_eq!(cache.reserved_decode_bytes, 0, "交付后预留必须归零");

        // 预留归还之后，同一份字节可以重新插入并交付。
        let second = cache.insert_async(Bytes::from(second_bytes));
        assert_eq!(cache.state(second), DecodeState::Pending);
        let state = poll_until(&mut cache, second);
        assert!(matches!(state, DecodeState::Ready(_)), "{state:?}");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.reserved_decode_bytes, 0);
    }

    /// D-11：并发上限——许可被占满时 `insert_async` 不得开始解码，释放许可后才投 blocking。
    #[test]
    fn async_decodes_wait_for_a_free_concurrency_permit() {
        use std::{
            sync::atomic::Ordering,
            time::{Duration, Instant},
        };

        let runtime = runtime();
        let mut cache = ResourceCache::for_test(
            (64, 64),
            DEFAULT_BYTE_BUDGET,
            Some(runtime.handle().clone()),
        );
        // 手动占满并发许可，模拟已有 `MAX_CONCURRENT_DECODES` 张图正在解码。
        let mut held = Vec::with_capacity(MAX_CONCURRENT_DECODES);
        for _ in 0..MAX_CONCURRENT_DECODES {
            held.push(
                cache
                    .decode_semaphore
                    .clone()
                    .try_acquire_owned()
                    .expect("新建缓存的许可必须全部可用"),
            );
        }

        let pending = cache.insert_async(Bytes::from(png(8, 8, [1, 2, 3, 255])));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            cache.decode_starts.load(Ordering::SeqCst),
            0,
            "没有空闲许可时不得开始解码"
        );
        assert!(cache.poll().is_empty(), "等待许可的任务不得交付");

        drop(held.pop());
        let state = poll_until(&mut cache, pending);
        assert!(matches!(state, DecodeState::Ready(_)), "{state:?}");
        assert_eq!(
            cache.decode_starts.load(Ordering::SeqCst),
            1,
            "拿到许可后恰好解码一次"
        );

        // 解码闭包在 send 之后才 drop 许可：稍等再断言许可已全部归还。
        drop(held);
        let deadline = Instant::now() + Duration::from_millis(100);
        while cache.decode_semaphore.available_permits() < MAX_CONCURRENT_DECODES
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            cache.decode_semaphore.available_permits(),
            MAX_CONCURRENT_DECODES,
            "解码结束后并发许可必须全部归还"
        );
    }

    /// D-11：同步 `insert` 也走同一套「先读头 + 准入，再解码」。
    #[test]
    fn sync_insert_uses_the_same_decode_admission() {
        let bytes = png(200, 200, [1, 2, 3, 255]);
        let page_bytes = 16 * 16 * 4;
        let budget = page_bytes + super::image::estimate_peak(200, 200) - 1;
        let mut cache = ResourceCache::for_test((16, 16), budget, None);

        let error = decode_cause(insert_error(&mut cache, &bytes));
        assert!(
            matches!(
                &error,
                DecodeCause::Capacity {
                    cause: CapacityCause::ByteBudget { .. },
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.reserved_decode_bytes, 0, "同步失败路径必须归还预留");
        assert_eq!(cache.stats().capacity_errors, 1);
    }

    /// D-11：PNG / JPEG / WebP 的「文件头尺寸」必须等于解码后的尺寸（准入估算的来源）。
    #[test]
    fn decode_peak_estimate_matches_header_dimensions() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::WebP,
        ] {
            let bytes = encoded(200, 100, [90, 120, 150, 255], format);
            let probed = super::image::probe_dimensions(&bytes).expect("probe header");
            assert_eq!(probed, (200, 100), "{format:?} 的头尺寸");
            let decoded = super::image::decode(&bytes, None).expect("decode");
            assert_eq!(
                (decoded.width, decoded.height),
                probed,
                "{format:?} 的 probe 尺寸必须等于解码尺寸"
            );
            assert!(
                super::image::estimate_peak(probed.0, probed.1) > 200 * 100 * 4,
                "{format:?} 的峰值估算必须覆盖 RGBA 输出下界"
            );
        }
    }
}

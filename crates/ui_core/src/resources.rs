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
//! # 解码不在 UI 线程（R12-15）
//!
//! [`ResourceCache::insert_async`] 只做**轻量**校验（字节长度、内容去重），把解码投到
//! `tokio::task::spawn_blocking`；[`ResourceCache::poll`] 每帧在 UI 线程把完成的结果装箱进图集。
//! 后台线程只拿到 `bytes::Bytes` 与一个 `oneshot` 发送端，**从不**触碰页缓冲、也从不持有
//! `&mut ResourceCache`。同步 [`ResourceCache::insert`] 保留（小图 / 没有 runtime 的宿主），
//! 两者的结果语义完全一致。
//!
//! 源图峰值约束在 `resources::image`：像素数上限 `MAX_SOURCE_PIXELS`（16 M px，取代单纯的
//! 单边 8192）+ 显式 `image::Limits`（`max_alloc` 256 MiB；命中 → [`DecodeCause::OutOfMemory`]）。
//!
//! # 内容去重 / 归属 / 预算（R12-16）
//!
//! - **去重**：`ContentKey = (len, ahash(bytes))` 索引 + 原始字节 memcmp（防碰撞）。同一份字节
//!   插入 N 次只占 1 块 tile，引用计数 N；命中时**不重新解码**。
//! - **归属**：[`ResourceCache::insert_owned`] 把引用登记到 [`WindowId`] 名下，
//!   [`ResourceCache::release_window`] 在该窗口关闭时（`app::window_set::close_window`）把引用
//!   一次性还回去（按 LRU epoch 顺序）。
//! - **预算**：字节预算（图片页 CPU 缓冲 + 去重表保留的源字节）超限 ⇒ [`DecodeCause::Capacity`]
//!   （[`CapacityCause::ByteBudget`]）。超预算时**不会**静默淘汰仍在引用的 tile；空闲条目在
//!   `unload` / `release_window` 里即时回收。[`ResourceCache::stats`] 是唯一的可观测面。
//!
//! # 应用层接线（现状）
//!
//! - `App` 持有进程级 [`ResourceCache`]（`App::new` 里 `ResourceCache::new(runtime)`）；带归属的
//!   插入用 [`ResourceCache::insert_owned`]。
//! - 关窗已接线：`app::window_set::close_window` 在窗口移除后调用
//!   [`ResourceCache::release_window`]（D-7）。
//! - 异步交付仍由宿主驱动：[`ResourceCache::poll`] 需要每帧调一次（帧钩子 / 平台泵）。本分支
//!   尚未把 `poll` 接进 `App` 帧循环，`insert_async` 的结果在接线前只能由调用方自己 poll。

use std::{fmt, num::NonZeroU32};

use bytes::Bytes;
use smol_str::SmolStr;
use tokio::sync::oneshot::{self, error::TryRecvError};

use crate::{
    error::EngineError,
    math::PhysicalRect,
    node::id::{Arena, ArenaKey},
    paint::atlas::{AtlasKind, AtlasTile},
    render::{AtlasAllocator, DirtyRects},
    resources::{
        cache::{ContentKey, DedupeEntry, Entry, Page},
        image::DecodedImage,
    },
    window::WindowId,
};

mod cache;
mod image;

/// 默认字节预算：图片页（16 MiB）+ 去重表保留的源字节。
///
/// 源字节是去重的代价（memcmp 需要原始缓冲）；图标 / logo 的压缩字节通常只有几十 KiB，
/// 256 MiB 的预算让「5000 行 × 不同图标」这种工作集也不会误报 [`DecodeCause::Capacity`]。
const DEFAULT_BYTE_BUDGET: u64 = 256 * 1024 * 1024;

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
    /// 字节预算（图片页缓冲 + 去重表保留的源字节）放不下该资源。
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
    /// 存活 tile 的内容区像素总和（缩放后的最终尺寸，不含 padding）。
    pub used_pixels: u64,
    /// CPU 已用字节：图片页缓冲 + 去重表保留的源字节。
    pub used_bytes: u64,
    /// tile 内容区像素 / 图片页像素（`0.0..=1.0`；没有图片页时 `0.0`）。
    pub page_occupancy: f32,
    /// 累计 [`DecodeCause::Capacity`] 失败次数（页满 / 预算不足的告警面）。
    pub capacity_errors: u64,
}

/// 图片资源缓存：字节 → 解码 → 图片图集 tile + 页缓冲。
///
/// **不持有 GPU 对象**：页缓冲由 `App::upload_image_page` 交给窗口渲染器（与字形页同一路径）。
/// 进程级一份（单 GPU 设备），由 `App` 持有。
pub struct ResourceCache {
    entries: Arena<Entry, ResourceId>,
    /// 内容去重索引：同一份字节只占一条 [`Entry`]，引用计数记在条目上。
    dedupe: ahash::HashMap<ContentKey, DedupeEntry>,
    /// 窗口归属：`insert_owned` 登记，`release_window` 一次性回收。
    owners: ahash::HashMap<WindowId, Vec<ResourceId>>,
    /// 在途的异步解码任务（`insert_async` 铸出 [`PendingResource`]）。
    pending: Arena<PendingJob, PendingResource>,
    /// `pending` 的键表（[`Arena`] 只提供按键访问，没有带键遍历）。
    pending_ids: Vec<PendingResource>,
    /// 图片页 + 分配器：**固定用最大档**（`MAX_ATLAS_PAGE` = 2048，见 `ensure_page`），
    /// 因此容纳能力与第一张图片的尺寸无关。没有插入过图片时不分配 CPU 页（16 MiB 量级）、
    /// GPU 纹理与 staging。
    page: Option<PageState>,
    /// 解码任务投递用的 tokio 句柄；`None` = 无 runtime（`insert_async` 退化为同步解码）。
    runtime: Option<tokio::runtime::Handle>,
    /// 去重表保留的源字节总和（`stats.used_bytes` 的一部分）。
    source_bytes: u64,
    /// 存活 tile 的内容区像素总和。
    used_pixels: u64,
    /// LRU 时钟：每次成功引用（新建 / 去重命中）`+1`。
    epoch: u64,
    /// 字节预算（图片页 + 去重表源字节）。
    byte_budget: u64,
    /// 累计 `Capacity` 失败次数。
    capacity_errors: u64,
}

/// 图片页 + 它的分配器（同生同灭：页尺寸锁定后不再变化）。
struct PageState {
    page: Page,
    allocator: AtlasAllocator,
}

/// 一个在途的异步解码任务。
struct PendingJob {
    /// 后台结果通道；`None` = 没有后台任务（同步路径已给出 `state`）。
    result: Option<oneshot::Receiver<Result<DecodedImage, DecodeCause>>>,
    /// 槽位当前状态（`Pending` = 还在等后台）。
    state: DecodeState,
    /// 源字节（后台完成后的去重 memcmp 用；就绪槽位不保留）。
    source: Bytes,
}

impl ResourceCache {
    /// 新建：异步解码用 `handle` 投递（`runtime` 由 `App` 持有，引擎内不建 runtime、
    /// 也不做 `Handle::current()`——在 runtime 之外会 panic）。
    pub(crate) fn new(handle: tokio::runtime::Handle) -> Self {
        Self::with_runtime(Some(handle))
    }

    /// 无 tokio runtime：`insert_async` **退化为同步解码**并立即给出 `DecodeState::Ready`。
    ///
    /// 只在测试里构造：生产路径的 `ResourceCache` 由 `App` 用 [`ResourceCache::new`] 建（`App`
    /// 永远持有 runtime），不带 runtime 的宿主直接用同步的 [`ResourceCache::insert`]。
    #[cfg(test)]
    pub(crate) fn new_unmanaged() -> Self {
        Self::with_runtime(None)
    }

    fn with_runtime(runtime: Option<tokio::runtime::Handle>) -> Self {
        Self {
            entries: Arena::new(),
            dedupe: ahash::HashMap::default(),
            owners: ahash::HashMap::default(),
            pending: Arena::new(),
            pending_ids: Vec::new(),
            page: None,
            runtime,
            source_bytes: 0,
            used_pixels: 0,
            epoch: 0,
            byte_budget: DEFAULT_BYTE_BUDGET,
            capacity_errors: 0,
        }
    }

    /// 仅测试：指定页尺寸 / 字节预算 / runtime 的构造器（跳过选档，便于穷尽容量与预算路径）。
    #[cfg(test)]
    fn for_test(
        page: (u32, u32),
        byte_budget: u64,
        runtime: Option<tokio::runtime::Handle>,
    ) -> Self {
        let mut cache = Self::with_runtime(runtime);
        cache.byte_budget = byte_budget;
        cache.open_page(page.0);
        cache
    }

    /// 仅测试：用指定页尺寸的小页构造。
    #[cfg(test)]
    pub(crate) fn with_page_for_test(page: (u32, u32)) -> Self {
        Self::for_test(page, DEFAULT_BYTE_BUDGET, None)
    }

    /// 建页（首次插入 / 测试注入）：整页先标记为脏，保证 GPU 侧新建纹理的其余区域是 0。
    fn open_page(&mut self, page: u32) {
        debug_assert!(self.page.is_none(), "图片页只建一次");
        let mut allocator = AtlasAllocator::with_page(AtlasKind::Image, (page, page));
        allocator.mark_written(full_page((page, page)));
        self.page = Some(PageState {
            page: Page::new(page, page),
            allocator,
        });
    }

    /// **图片页固定开最大档**（P1-2.7）。
    ///
    /// 旧实现用 `page_for_tile(width, height)` 按**第一张**图片选档并永久锁定：先加载 16×16 图标
    /// ⇒ 页 = 512² ⇒ 之后任何 >510 px 的图必然 `Capacity`，而错误文案还让「unload 后重试」
    /// （页尺寸永不变，重试无效）。加载顺序决定整个进程的图片上限是不可接受的隐式契约。
    ///
    /// 固定最大档后容纳能力与插入顺序无关；懒分配语义不变（没有图片就仍不占内存），代价是需要
    /// 图片时一次性占 16 MiB —— 这本来就是文档里的图片页上限。更省内存的做法是 Skia/Impeller 式的
    /// 多页图集（`AtlasTile` 增 `page`、三平台 `texture_index` 改纹理数组），属契约变更，留作后续。
    fn ensure_page(&mut self) {
        if self.page.is_none() {
            self.open_page(crate::render::atlas::MAX_ATLAS_PAGE);
        }
    }

    /// 解码 `bytes` 并装箱，返回句柄、tile 与像素尺寸。
    ///
    /// 同一内容第二次插入是**去重命中**：不重新解码，返回同一个 [`ResourceImage`]，引用计数 +1。
    ///
    /// # Errors
    /// [`EngineError::ResourceDecode`]：`UnsupportedFormat` / `Corrupt` / `SourceTooLarge` /
    /// `TooLarge` / `OutOfMemory` / `Capacity`。
    pub fn insert(&mut self, bytes: &[u8]) -> Result<ResourceImage, EngineError> {
        self.insert_inner(bytes)
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    fn insert_inner(&mut self, bytes: &[u8]) -> Result<ResourceImage, DecodeCause> {
        image::check_source_bytes(bytes.len())?;
        if let Some(image) = self.dedupe_hit(bytes) {
            return Ok(image);
        }
        let decoded = image::decode(bytes)?;
        self.insert_decoded(Bytes::copy_from_slice(bytes), decoded)
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

    /// 把解码投到 tokio 阻塞池，**立即**返回句柄（UI 线程只做轻量校验）。
    ///
    /// 结果通过 [`ResourceCache::poll`] 交付；[`ResourceCache::state`] 可随时查询。
    /// 缓存在测试 / 无 runtime 模式下（`new_unmanaged`）退化为同步解码并立即 `Ready`。
    pub fn insert_async(&mut self, bytes: Bytes) -> PendingResource {
        if let Err(cause) = image::check_source_bytes(bytes.len()) {
            return self.push_pending(PendingJob {
                result: None,
                state: DecodeState::Failed(cause),
                source: Bytes::new(),
            });
        }
        if let Some(image) = self.dedupe_hit(&bytes) {
            // 去重命中：不需要解码，立刻可用。
            return self.push_pending(PendingJob {
                result: None,
                state: DecodeState::Ready(image),
                source: Bytes::new(),
            });
        }
        let Some(runtime) = self.runtime.clone() else {
            let state = match image::decode(&bytes) {
                Ok(decoded) => self.finish_decoded(bytes, decoded),
                Err(cause) => DecodeState::Failed(cause),
            };
            return self.push_pending(PendingJob {
                result: None,
                state,
                source: Bytes::new(),
            });
        };
        let (sender, receiver) = oneshot::channel();
        let source = bytes.clone();
        runtime.spawn_blocking(move || {
            // 后台线程只解码字节；图集、页缓冲、LRU/预算全部留在 UI 线程。
            let _ = sender.send(image::decode(&source));
        });
        self.push_pending(PendingJob {
            result: Some(receiver),
            state: DecodeState::Pending,
            source: bytes,
        })
    }

    /// 每帧在 UI 线程调用一次：把已完成的异步解码装箱进图集。
    ///
    /// 返回**本次**就绪 / 失败的项（每项只交付一次；交付后 `PendingResource` 被回收）。
    /// 后台线程从不触碰图集，装箱（去重 / 预算 / 分配 / 写页）只在这里发生。
    pub fn poll(&mut self) -> Vec<(PendingResource, DecodeState)> {
        let ids: Vec<PendingResource> = self.pending_ids.clone();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            // 1. 同步就绪（去重命中 / 轻量校验失败 / 无 runtime 的退化路径）。
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
                continue;
            };
            let source = self
                .pending
                .get(id)
                .map_or_else(Bytes::new, |job| job.source.clone());
            let state = match completion {
                Ok(decoded) => self.finish_decoded(source, decoded),
                Err(cause) => DecodeState::Failed(cause),
            };
            self.pending.remove(id);
            out.push((id, state));
        }
        let live: Vec<PendingResource> = {
            let pending = &self.pending;
            self.pending_ids
                .iter()
                .copied()
                .filter(|id| pending.get(*id).is_some())
                .collect()
        };
        self.pending_ids = live;
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

    fn push_pending(&mut self, job: PendingJob) -> PendingResource {
        let id = self.pending.insert_with(|_| job);
        self.pending_ids.push(id);
        id
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
    fn finish_decoded(&mut self, source: Bytes, decoded: DecodedImage) -> DecodeState {
        if let Some(image) = self.dedupe_hit(&source) {
            return DecodeState::Ready(image);
        }
        match self.insert_decoded(source, decoded) {
            Ok(image) => DecodeState::Ready(image),
            Err(cause) => DecodeState::Failed(cause),
        }
    }

    /// 去重命中：同一份字节已经有一个 tile ⇒ 引用计数 +1，返回既有结果（**不重新解码**）。
    fn dedupe_hit(&mut self, bytes: &[u8]) -> Option<ResourceImage> {
        let key = ContentKey::new(bytes);
        let existing = self.dedupe.get(&key)?;
        // 64 bit 哈希碰撞：必须 memcmp 原始字节才能认定「同一内容」。
        if existing.bytes.as_ref() != bytes {
            return None;
        }
        let id = existing.id;
        let epoch = self.next_epoch();
        let Some(entry) = self.entries.get_mut(id) else {
            // 索引里残留了已回收的映射（防御性清理）。
            self.dedupe.remove(&key);
            return None;
        };
        entry.refcount = entry.refcount.saturating_add(1);
        entry.last_used = epoch;
        Some(entry.image)
    }

    /// 解码结果 → 图集：预算检查 → 装箱写页 → 去重索引。
    fn insert_decoded(
        &mut self,
        source: Bytes,
        decoded: DecodedImage,
    ) -> Result<ResourceImage, DecodeCause> {
        let key = ContentKey::new(&source);
        self.ensure_page();
        self.ensure_budget(decoded.width, decoded.height, source.len() as u64)?;
        let image = self.place(decoded, key)?;
        self.source_bytes = self.source_bytes.saturating_add(source.len() as u64);
        if let Some(replaced) = self.dedupe.insert(
            key,
            DedupeEntry {
                id: image.id,
                bytes: source,
            },
        ) {
            // 哈希碰撞导致同一个 key 覆盖了另一条内容：旧源字节不再保留。
            self.source_bytes = self
                .source_bytes
                .saturating_sub(replaced.bytes.len() as u64);
        }
        Ok(image)
    }

    /// 字节预算检查（图片页 CPU 缓冲 + 去重表保留的源字节）。
    ///
    /// 超预算时**不得**静默淘汰仍在引用的 tile（那会破坏「tile 恒有效」）。空闲条目在
    /// `unload` / `release_window` 里已经即时回收，所以这里没有可淘汰对象，直接报
    /// [`CapacityCause::ByteBudget`]。
    fn ensure_budget(
        &mut self,
        width: u32,
        height: u32,
        additional: u64,
    ) -> Result<(), DecodeCause> {
        let used = self.used_bytes();
        if used.saturating_add(additional) <= self.byte_budget {
            return Ok(());
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

    /// 把解码结果装箱进图片页（预算检查已由调用方完成）。
    ///
    /// 失败只有一种：页里放不下（[`CapacityCause::PageFull`]）；失败**不**改变既有条目与代际。
    fn place(
        &mut self,
        decoded: DecodedImage,
        key: ContentKey,
    ) -> Result<ResourceImage, DecodeCause> {
        let (width, height) = (decoded.width, decoded.height);
        let epoch = self.next_epoch();
        let (allocation, page) = {
            let state = self.page.as_mut().expect("page is ensured before placing");
            let page = state.page.dimensions();
            (state.allocator.allocate((width, height)), page)
        };
        let Some(allocation) = allocation else {
            self.capacity_errors = self.capacity_errors.saturating_add(1);
            return Err(DecodeCause::Capacity {
                width,
                height,
                page,
                cause: CapacityCause::PageFull,
            });
        };
        debug_assert_eq!(allocation.tile.atlas, AtlasKind::Image);
        {
            let state = self.page.as_mut().expect("page is ensured before placing");
            // padding 复用时可能残留上一块的内容：先清整块（含 padding），再写内容区。
            state.page.clear_rect(allocation.padded);
            state.allocator.mark_written(allocation.padded);
            state
                .page
                .write(allocation.content, &decoded.bgra, width as usize * 4);
        }
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
        });
        self.used_pixels = self
            .used_pixels
            .saturating_add(u64::from(width) * u64::from(height));
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
        let entries = self.entries.len();
        ResourceStats {
            entries,
            tiles: entries,
            used_pixels: self.used_pixels,
            used_bytes: self.used_bytes(),
            page_occupancy: if page_pixels == 0 {
                0.0
            } else {
                self.used_pixels as f32 / page_pixels as f32
            },
            capacity_errors: self.capacity_errors,
        }
    }

    /// 存活资源数（引用计数 > 0 的条目数；图片被多次去重命中时仍是 1）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空（快照路径的上传开关）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 图片页的当前内容（`page.0 * page.1 * 4` 字节，BGRA8 预乘 sRGB）；没插过图片时 `None`。
    pub(crate) fn page_pixels(&self) -> Option<((u32, u32), &[u8])> {
        self.page
            .as_ref()
            .map(|state| (state.page.dimensions(), state.page.bytes()))
    }

    /// 取走自上次上传以来被写脏的**矩形**（首次调用返回整页）；没有图片页时是空集。
    pub(crate) fn take_dirty_rects(&mut self) -> DirtyRects {
        self.page
            .as_mut()
            .map_or_else(DirtyRects::default, |state| {
                state.allocator.take_dirty_rects()
            })
    }

    /// 回收一条引用归零的资源：退还 tile、清零页内区域（含 padding）、删去重索引。
    fn reclaim(&mut self, id: ResourceId) {
        let Some(entry) = self.entries.remove(id) else {
            return;
        };
        let owned_key = self
            .dedupe
            .get(&entry.key)
            .is_some_and(|dedupe| dedupe.id == id)
            .then_some(entry.key);
        if let Some(key) = owned_key
            && let Some(dedupe) = self.dedupe.remove(&key)
        {
            self.source_bytes = self.source_bytes.saturating_sub(dedupe.bytes.len() as u64);
        }
        self.used_pixels = self
            .used_pixels
            .saturating_sub(u64::from(entry.image.width) * u64::from(entry.image.height));
        if let Some(state) = self.page.as_mut() {
            state.allocator.deallocate(entry.alloc);
            state.page.clear_rect(entry.padded);
            state.allocator.mark_written(entry.padded);
        }
    }

    fn next_epoch(&mut self) -> u64 {
        self.epoch = self.epoch.saturating_add(1);
        self.epoch
    }

    fn page_dimensions(&self) -> (u32, u32) {
        self.page
            .as_ref()
            .map_or((0, 0), |state| state.page.dimensions())
    }

    fn used_bytes(&self) -> u64 {
        let (width, height) = self.page_dimensions();
        u64::from(width) * u64::from(height) * 4 + self.source_bytes
    }
}

/// 整页矩形（`mark_written` 的入参）。
fn full_page(page: (u32, u32)) -> PhysicalRect {
    PhysicalRect::new(0, 0, page.0 as i32, page.1 as i32)
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use bytes::Bytes;

    use super::{
        CapacityCause, DecodeCause, DecodeState, PendingResource, ResourceCache, ResourceId,
        ResourceImage, ResourceStats,
    };
    use crate::{
        error::EngineError,
        math::PhysicalRect,
        paint::atlas::AtlasKind,
        resources::image::{MAX_SOURCE_BYTES, MAX_SOURCE_DIM, MAX_SOURCE_PIXELS},
        window::WindowId,
    };

    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let pixels: Vec<u8> = std::iter::repeat_n(rgba, (width * height) as usize)
            .flatten()
            .collect();
        let image = image::RgbaImage::from_raw(width, height, pixels).expect("raw");
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
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

    #[test]
    fn insert_reports_capacity_with_sizes() {
        // 8×8 的页：一块 4×4（+2 padding = 6×6）之后放不下第二块。
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let _ = inserted(&mut cache, [7, 7, 7, 255]);
        let error = decode_cause(insert_error(&mut cache, &png(4, 4, [8, 8, 8, 255])));
        assert_eq!(
            error,
            DecodeCause::Capacity {
                width: 4,
                height: 4,
                page: (8, 8),
                cause: CapacityCause::PageFull,
            }
        );
        assert_eq!(cache.len(), 1, "容量失败不改变既有条目");
        assert_eq!(cache.stats().capacity_errors, 1, "容量失败要计数（诊断面）");
    }

    #[test]
    fn capacity_failure_does_not_bump_generation() {
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let image = inserted(&mut cache, [9, 9, 9, 255]);
        let generation = image.tile.generation;
        let _ = insert_error(&mut cache, &png(4, 4, [1, 1, 1, 255]));

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

    /// R12-16 预算：超预算的插入是 `Capacity`（`ByteBudget`），`unload` 之后能再插。
    #[test]
    fn byte_budget_reports_capacity_and_recovers_after_unload() {
        let first_bytes = png(4, 4, [1, 2, 3, 255]);
        let second_bytes = png(4, 4, [4, 5, 6, 255]);
        let page_bytes = 16 * 16 * 4;
        // 正好放得下一张（页 + 第一张源字节），放不下两张。
        let budget = page_bytes + first_bytes.len() as u64 + second_bytes.len() as u64 - 1;
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

        assert!(cache.unload(first.id));
        let second = cache.insert(&second_bytes).expect("unload 之后预算有空间");
        assert_ne!(second.id, first.id);
        assert_eq!(cache.stats().capacity_errors, 1, "成功插入不再累计失败");
    }

    #[test]
    fn stats_track_page_usage_and_entries() {
        let mut cache = cache();
        assert_eq!(
            cache.stats(),
            ResourceStats {
                entries: 0,
                tiles: 0,
                used_pixels: 0,
                used_bytes: 16 * 16 * 4,
                page_occupancy: 0.0,
                capacity_errors: 0,
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
}

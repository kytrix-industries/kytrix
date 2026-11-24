use std::{fmt, num::NonZeroU32, sync::Arc};

use bytes::Bytes;
use smallvec::SmallVec;
use smol_str::SmolStr;
use tokio::sync::{
    Semaphore,
    oneshot::{self, error::TryRecvError},
};

use crate::{
    error::EngineError,
    math::{DevicePixelRatio, DeviceSize, LogicalPx, LogicalSize, dp},
    node::id::{Arena, ArenaKey},
    paint::atlas::{AtlasKind, AtlasTile},
    platform::dispatcher::Dispatcher,
    render::{DirtyRects, ImageAtlasPager},
    resources::{
        cache::{ContentKey, DedupeEntry, Entry, Page, Variant},
        image::{DecodedImage, MAX_TILE_DIM, SourceColor, SourceKind},
    },
    window::WindowId,
};

mod cache;
mod image;
mod svg;

const DEFAULT_BYTE_BUDGET: u64 = 320 * 1024 * 1024;

const MAX_CONCURRENT_DECODES: usize = 2;

const VARIANT_GRID: u32 = 4;

const MAX_VARIANTS_PER_RESOURCE: usize = 8;

const VARIANT_BYTES_PER_PIXEL: u64 = 4;

const VARIANT_PEAK_MARGIN: u64 = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageDiagnostic {
    VariantUnavailable {
        resource: ResourceId,
        width: u32,
        height: u32,
        cause: DecodeCause,
    },
}

impl ImageDiagnostic {
    #[must_use]
    pub const fn resource(&self) -> ResourceId {
        match self {
            Self::VariantUnavailable { resource, .. } => *resource,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VariantState {
    Ready { created: bool },
    Failed { cause: DecodeCause },
}

fn checked_intrinsic(intrinsic: LogicalSize) -> Result<LogicalSize, EngineError> {
    let valid = intrinsic.width.is_finite()
        && intrinsic.width > LogicalPx::ZERO
        && intrinsic.height.is_finite()
        && intrinsic.height > LogicalPx::ZERO;
    if valid {
        Ok(intrinsic)
    } else {
        Err(EngineError::InvalidImageIntrinsic {
            width: intrinsic.width.value(),
            height: intrinsic.height.value(),
        })
    }
}

fn quantize_variant_target(target: DeviceSize) -> DeviceSize {
    let clamp = |value: i32| value.clamp(1, MAX_TILE_DIM as i32) as u32;
    let width = clamp(target.width.value());
    let height = clamp(target.height.value());
    let longest = width.max(height);
    let quantized = longest.div_ceil(VARIANT_GRID) * VARIANT_GRID;
    if quantized == longest {
        return DeviceSize::new(dp(width as i32), dp(height as i32));
    }
    let scale = quantized as f32 / longest as f32;
    let scaled = |value: u32| ((value as f32 * scale).ceil() as i32).clamp(1, MAX_TILE_DIM as i32);
    DeviceSize::new(dp(scaled(width)), dp(scaled(height)))
}

fn lod_sizes(width: u32, height: u32) -> SmallVec<[(u32, u32); 2]> {
    let mut out: SmallVec<[(u32, u32); 2]> = SmallVec::new();
    for divisor in [2u32, 4] {
        let lod_width = width.div_ceil(divisor).clamp(1, width);
        let lod_height = height.div_ceil(divisor).clamp(1, height);
        let size = (lod_width, lod_height);
        if size != (width, height) && !out.contains(&size) {
            out.push(size);
        }
    }
    out
}

fn select_exact(entry: &Entry, target: DeviceSize) -> Option<usize> {
    match entry.kind {
        ImageResourceKind::Bitmap { .. } => select_bitmap_lod(entry, target),
        ImageResourceKind::Vector { .. } => {
            let quantized = quantize_variant_target(target);
            entry
                .variants
                .iter()
                .position(|variant| variant.target == quantized)
        }
    }
}

fn select_variant(entry: &Entry, target: DeviceSize) -> Option<usize> {
    if let Some(index) = select_exact(entry, target) {
        return Some(index);
    }
    let quantized = quantize_variant_target(target);
    let target_width = quantized.width.value();
    let target_height = quantized.height.value();
    let fitting = entry
        .variants
        .iter()
        .enumerate()
        .filter(|(_, variant)| {
            variant.target.width.value() <= target_width
                && variant.target.height.value() <= target_height
        })
        .max_by_key(|(_, variant)| variant.pixels);
    if let Some((index, _)) = fitting {
        return Some(index);
    }
    entry
        .variants
        .iter()
        .enumerate()
        .min_by_key(|(_, variant)| variant.pixels)
        .map(|(index, _)| index)
}

fn select_bitmap_lod(entry: &Entry, target: DeviceSize) -> Option<usize> {
    let target_width = target.width.value();
    let target_height = target.height.value();
    let fitting = entry
        .variants
        .iter()
        .enumerate()
        .filter(|(_, variant)| {
            variant.target.width.value() >= target_width
                && variant.target.height.value() >= target_height
        })
        .min_by_key(|(_, variant)| variant.pixels);
    if let Some((index, _)) = fitting {
        return Some(index);
    }
    entry
        .variants
        .iter()
        .enumerate()
        .max_by_key(|(_, variant)| variant.pixels)
        .map(|(index, _)| index)
}

fn select_eviction_victim(entry: &Entry, frame_epoch: u64) -> Option<usize> {
    let unpinned = |variant: &&Variant| variant.pinned_frame < frame_epoch;
    entry
        .variants
        .iter()
        .enumerate()
        .filter(|(_, variant)| unpinned(variant) && !variant.exposed)
        .min_by_key(|(_, variant)| variant.last_used)
        .or_else(|| {
            entry
                .variants
                .iter()
                .enumerate()
                .filter(|(_, variant)| unpinned(variant))
                .min_by_key(|(_, variant)| variant.last_used)
        })
        .map(|(index, _)| index)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageResourceKind {
    Bitmap { native: DeviceSize },
    Vector { intrinsic: LogicalSize },
}

impl ImageResourceKind {
    #[must_use]
    pub fn intrinsic(self, dpr: DevicePixelRatio) -> LogicalSize {
        match self {
            Self::Bitmap { native } => dpr.to_logical_size(native),
            Self::Vector { intrinsic } => intrinsic,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceImage {
    id: ResourceId,
    kind: ImageResourceKind,
}

impl ResourceImage {
    #[must_use]
    pub const fn id(self) -> ResourceId {
        self.id
    }

    #[must_use]
    pub const fn kind(self) -> ImageResourceKind {
        self.kind
    }

    #[must_use]
    pub fn intrinsic(self, dpr: DevicePixelRatio) -> LogicalSize {
        self.kind.intrinsic(dpr)
    }

    #[must_use]
    pub fn variant_for(self, cache: &ResourceCache, target: DeviceSize) -> Option<AtlasTile> {
        cache.variant_for(self, target)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId {
    index: u32,
    generation: NonZeroU32,
}

impl ResourceId {
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    #[must_use]
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PendingResource {
    index: u32,
    generation: NonZeroU32,
}

impl PendingResource {
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self {
        match NonZeroU32::new(generation) {
            Some(generation) => Self { index, generation },
            None => panic!("PendingResource 的代际不得为 0"),
        }
    }

    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

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

#[derive(Clone, Debug, PartialEq)]
pub enum DecodeState {
    Pending,
    Ready(ResourceImage),
    Failed(DecodeCause),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityCause {
    PageFull,
    ByteBudget { used_bytes: u64, limit_bytes: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeCause {
    UnsupportedFormat {
        format: SmolStr,
    },
    Corrupt {
        format: SmolStr,
        detail: SmolStr,
    },
    SourceTooLarge {
        bytes: u64,
        limit: u64,
    },
    TooLarge {
        width: u32,
        height: u32,
        pixels: u64,
        limit: u64,
        dim_limit: u32,
    },
    OutOfMemory {
        limit_bytes: u64,
    },
    Capacity {
        width: u32,
        height: u32,
        page: (u32, u32),
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResourceStats {
    pub entries: usize,
    pub tiles: usize,
    pub pages: usize,
    pub used_pixels: u64,
    pub used_bytes: u64,
    pub page_occupancy: f32,
    pub capacity_errors: u64,
    pub color_converted: u64,
    pub color_fallback: u64,
}

pub struct ResourceCache {
    entries: Arena<Entry, ResourceId>,
    dedupe: ahash::HashMap<ContentKey, DedupeEntry>,
    owners: ahash::HashMap<WindowId, Vec<ResourceId>>,
    pending: Arena<PendingJob, PendingResource>,
    pending_ids: Vec<PendingResource>,
    pager: ImageAtlasPager,
    page_buffers: Vec<Option<Page>>,
    page_generations: Vec<u32>,
    runtime: Option<tokio::runtime::Handle>,
    dispatcher: Arc<Dispatcher>,
    decode_semaphore: Arc<Semaphore>,
    reserved_decode_bytes: u64,
    source_bytes: u64,
    used_pixels: u64,
    variant_count: usize,
    frame_epoch: u64,
    epoch: u64,
    byte_budget: u64,
    capacity_errors: u64,
    color_converted: u64,
    color_fallback: u64,
    #[cfg(test)]
    decode_starts: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    panic_decode_next: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    lod_levels: u8,
}

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
        self.dispatcher.wake();
    }
}

struct PendingJob {
    result: Option<oneshot::Receiver<Result<DecodedImage, DecodeCause>>>,
    state: DecodeState,
    source: Bytes,
    reservation: u64,
    key: ContentKey,
    owner: Option<WindowId>,
}

impl PendingJob {
    fn settled(state: DecodeState, owner: Option<WindowId>) -> Self {
        Self {
            result: None,
            state,
            source: Bytes::new(),
            reservation: 0,
            key: ContentKey::empty(),
            owner,
        }
    }
}

impl ResourceCache {
    pub(crate) fn new(handle: tokio::runtime::Handle, dispatcher: Arc<Dispatcher>) -> Self {
        Self::with_runtime(Some(handle), dispatcher)
    }

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
            variant_count: 0,
            frame_epoch: 1,
            epoch: 0,
            byte_budget: DEFAULT_BYTE_BUDGET,
            capacity_errors: 0,
            color_converted: 0,
            color_fallback: 0,
            #[cfg(test)]
            decode_starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            panic_decode_next: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            lod_levels: 2,
        }
    }

    #[cfg(all(test, windows))]
    pub(crate) fn without_lods(mut self) -> Self {
        self.lod_levels = 0;
        self
    }

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

    #[cfg(test)]
    pub(crate) fn with_page_for_test(page: (u32, u32)) -> Self {
        Self::for_test(page, DEFAULT_BYTE_BUDGET, None)
    }

    #[cfg(test)]
    pub(crate) fn insert(&mut self, bytes: &[u8]) -> Result<ResourceImage, EngineError> {
        self.insert_bitmap(bytes, None)
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    pub(crate) fn insert_owned(
        &mut self,
        owner: WindowId,
        bytes: &[u8],
    ) -> Result<ResourceImage, EngineError> {
        self.insert_bitmap(bytes, Some(owner))
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    fn insert_bitmap(
        &mut self,
        bytes: &[u8],
        owner: Option<WindowId>,
    ) -> Result<ResourceImage, DecodeCause> {
        image::check_source_bytes(bytes.len())?;
        let key = ContentKey::new(bytes, SourceKind::Bitmap, None);
        if let Some(image) = self.dedupe_hit(&key, bytes, owner) {
            return Ok(image);
        }
        let (width, height) = image::probe_dimensions(bytes)?;
        let reservation = self.admit_decode(width, height, bytes.len())?;
        let decoded = match image::decode(bytes) {
            Ok(decoded) => decoded,
            Err(cause) => {
                self.release_reservation(reservation);
                return Err(cause);
            }
        };
        self.release_reservation(reservation);
        self.commit_bitmap(Bytes::copy_from_slice(bytes), decoded, key, owner)
    }

    #[cfg(test)]
    pub(crate) fn insert_svg(
        &mut self,
        bytes: &[u8],
        intrinsic: LogicalSize,
    ) -> Result<ResourceImage, EngineError> {
        let intrinsic = checked_intrinsic(intrinsic)?;
        self.insert_vector(bytes, intrinsic, None)
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    pub(crate) fn insert_owned_svg(
        &mut self,
        owner: WindowId,
        bytes: &[u8],
        intrinsic: LogicalSize,
    ) -> Result<ResourceImage, EngineError> {
        let intrinsic = checked_intrinsic(intrinsic)?;
        self.insert_vector(bytes, intrinsic, Some(owner))
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    fn insert_vector(
        &mut self,
        bytes: &[u8],
        intrinsic: LogicalSize,
        owner: Option<WindowId>,
    ) -> Result<ResourceImage, DecodeCause> {
        image::check_source_bytes(bytes.len())?;
        let key = ContentKey::new(bytes, SourceKind::Svg, Some(intrinsic));
        if let Some(image) = self.dedupe_hit(&key, bytes, owner) {
            return Ok(image);
        }
        svg::check_source(bytes)?;
        let image = self.insert_resource(
            Bytes::copy_from_slice(bytes),
            ImageResourceKind::Vector { intrinsic },
            key,
            owner,
        );
        Ok(image)
    }

    #[cfg(test)]
    pub(crate) fn insert_async(&mut self, bytes: Bytes) -> PendingResource {
        self.insert_bitmap_async(bytes, None)
    }

    pub(crate) fn insert_owned_async(&mut self, owner: WindowId, bytes: Bytes) -> PendingResource {
        self.insert_bitmap_async(bytes, Some(owner))
    }

    pub(crate) fn insert_owned_svg_async(
        &mut self,
        owner: WindowId,
        bytes: Bytes,
        intrinsic: LogicalSize,
    ) -> Result<PendingResource, EngineError> {
        let image = self.insert_owned_svg(owner, &bytes, intrinsic)?;
        Ok(self.push_pending(PendingJob::settled(DecodeState::Ready(image), Some(owner))))
    }

    fn insert_bitmap_async(&mut self, bytes: Bytes, owner: Option<WindowId>) -> PendingResource {
        if let Err(cause) = image::check_source_bytes(bytes.len()) {
            return self.push_failed(cause, owner);
        }
        let key = ContentKey::new(&bytes, SourceKind::Bitmap, None);
        if let Some(image) = self.dedupe_hit(&key, &bytes, owner) {
            return self.push_ready(image, owner);
        }
        let (width, height) = match image::probe_dimensions(&bytes) {
            Ok(dimensions) => dimensions,
            Err(cause) => return self.push_failed(cause, owner),
        };
        let reservation = match self.admit_decode(width, height, bytes.len()) {
            Ok(reservation) => reservation,
            Err(cause) => return self.push_failed(cause, owner),
        };
        let Some(runtime) = self.runtime.clone() else {
            self.release_reservation(reservation);
            let state = match image::decode(&bytes) {
                Ok(decoded) => self.finish_bitmap(bytes, decoded, key, owner),
                Err(cause) => DecodeState::Failed(cause),
            };
            return self.push_pending(PendingJob::settled(state, owner));
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
                let _permit = permit;
                let mut guard = DecodeGuard::new(sender, dispatcher);
                #[cfg(test)]
                {
                    if panic_decode_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        panic!("injected decode worker panic (RD-1 test seam)");
                    }
                    decode_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                let result = image::decode(&source);
                guard.deliver(result);
            });
        });
        self.push_pending(PendingJob {
            result: Some(receiver),
            state: DecodeState::Pending,
            source: bytes,
            reservation,
            key,
            owner,
        })
    }

    pub(crate) fn poll(&mut self) -> Vec<(PendingResource, DecodeState)> {
        let ids = std::mem::take(&mut self.pending_ids);
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if self.pending.get(id).is_none() {
                continue;
            }
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
            let Some(completion) = self.take_completion(id) else {
                self.pending_ids.push(id);
                continue;
            };
            let Some(job) = self.pending.remove(id) else {
                continue;
            };
            self.release_reservation(job.reservation);
            let state = match completion {
                Ok(decoded) => self.finish_bitmap(job.source, decoded, job.key, job.owner),
                Err(cause) => DecodeState::Failed(cause),
            };
            out.push((id, state));
        }
        out
    }

    #[must_use]
    pub fn state(&self, pending: PendingResource) -> DecodeState {
        self.pending
            .get(pending)
            .map_or(DecodeState::Pending, |job| job.state.clone())
    }

    #[must_use]
    pub fn image(&self, id: ResourceId) -> Option<ResourceImage> {
        self.entries.get(id).map(|entry| ResourceImage {
            id,
            kind: entry.kind,
        })
    }

    #[must_use]
    pub fn variant_for(&self, image: ResourceImage, target: DeviceSize) -> Option<AtlasTile> {
        let entry = self.entries.get(image.id)?;
        let index = select_variant(entry, target)?;
        entry.variants.get(index).map(|variant| variant.tile)
    }

    pub(crate) fn begin_variant_frame(&mut self) {
        self.frame_epoch = self.frame_epoch.wrapping_add(1);
    }

    pub(crate) fn ensure_variant(
        &mut self,
        image: ResourceImage,
        target: DeviceSize,
    ) -> Option<VariantState> {
        let (kind, key) = {
            let entry = self.entries.get(image.id)?;
            if let Some(index) = select_exact(entry, target) {
                let epoch = self.next_epoch();
                let frame_epoch = self.frame_epoch;
                if let Some(entry) = self.entries.get_mut(image.id)
                    && let Some(variant) = entry.variants.get_mut(index)
                {
                    variant.last_used = epoch;
                    variant.pinned_frame = frame_epoch;
                }
                return Some(VariantState::Ready { created: false });
            }
            if matches!(entry.kind, ImageResourceKind::Bitmap { .. }) {
                return None;
            }
            (entry.kind, entry.key)
        };
        let quantized = quantize_variant_target(target);
        let ImageResourceKind::Vector { .. } = kind else {
            return None;
        };
        let source = self.dedupe.get(&key)?.bytes.clone();
        let width = u32::try_from(quantized.width.value()).ok()?.max(1);
        let height = u32::try_from(quantized.height.value()).ok()?.max(1);
        let reservation = match self.admit_variant(width, height) {
            Ok(reservation) => reservation,
            Err(cause) => {
                self.pin_fallback(image.id, target);
                return Some(VariantState::Failed { cause });
            }
        };
        let decoded = match svg::decode(&source, quantized) {
            Ok(decoded) => decoded,
            Err(cause) => {
                self.release_reservation(reservation);
                self.pin_fallback(image.id, target);
                return Some(VariantState::Failed { cause });
            }
        };
        let variant = match self.place_variant(&decoded, quantized, true) {
            Ok(variant) => variant,
            Err(cause) => {
                self.release_reservation(reservation);
                self.pin_fallback(image.id, target);
                return Some(VariantState::Failed { cause });
            }
        };
        self.release_reservation(reservation);
        let epoch = self.next_epoch();
        let entry = self.entries.get_mut(image.id)?;
        entry.last_used = epoch;
        entry.variants.push(variant);
        self.variant_count = self.variant_count.saturating_add(1);
        self.trim_variants(image.id);
        Some(VariantState::Ready { created: true })
    }

    pub(crate) fn protect_variant(&mut self, image: ResourceImage, target: DeviceSize) {
        let Some(index) = self
            .entries
            .get(image.id)
            .and_then(|entry| select_variant(entry, target))
        else {
            return;
        };
        let epoch = self.next_epoch();
        let pinned_frame = self.frame_epoch.wrapping_add(1);
        if let Some(variant) = self
            .entries
            .get_mut(image.id)
            .and_then(|entry| entry.variants.get_mut(index))
        {
            variant.last_used = epoch;
            variant.pinned_frame = pinned_frame;
            variant.exposed = true;
        }
    }

    fn pin_fallback(&mut self, id: ResourceId, target: DeviceSize) {
        let Some(index) = self
            .entries
            .get(id)
            .and_then(|entry| select_variant(entry, target))
        else {
            return;
        };
        let epoch = self.next_epoch();
        let frame_epoch = self.frame_epoch;
        if let Some(variant) = self
            .entries
            .get_mut(id)
            .and_then(|entry| entry.variants.get_mut(index))
        {
            variant.last_used = epoch;
            variant.pinned_frame = frame_epoch;
        }
    }

    fn trim_variants(&mut self, id: ResourceId) {
        loop {
            let Some(entry) = self.entries.get(id) else {
                return;
            };
            if entry.variants.len() <= MAX_VARIANTS_PER_RESOURCE {
                return;
            }
            let victim = select_eviction_victim(entry, self.frame_epoch);
            let Some(index) = victim else {
                return;
            };
            self.evict_variant(id, index);
        }
    }

    fn evict_variant(&mut self, id: ResourceId, index: usize) {
        let Some(entry) = self.entries.get_mut(id) else {
            return;
        };
        if index >= entry.variants.len() {
            return;
        }
        let variant = entry.variants.remove(index);
        self.used_pixels = self.used_pixels.saturating_sub(variant.pixels);
        self.variant_count = self.variant_count.saturating_sub(1);
        let page = variant.tile.page;
        if let Some(buffer) = self
            .page_buffers
            .get_mut(usize::from(page))
            .and_then(Option::as_mut)
        {
            buffer.clear_rect(variant.padded);
        }
        self.pager.deallocate(page, variant.alloc);
        self.pager.mark_written(page, variant.padded);
    }

    fn admit_variant(&mut self, width: u32, height: u32) -> Result<u64, DecodeCause> {
        let (page_width, page_height) = self.page_dimensions();
        let page_bytes = u64::from(page_width) * u64::from(page_height) * 4;
        let estimated_content = (width.min(MAX_TILE_DIM), height.min(MAX_TILE_DIM));
        let pending_page_bytes = if self.pager.would_open_new_page(estimated_content) {
            page_bytes
        } else {
            0
        };
        let pixels = u64::from(width) * u64::from(height);
        let reservation = pixels
            .saturating_mul(VARIANT_BYTES_PER_PIXEL)
            .saturating_add(VARIANT_PEAK_MARGIN)
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

    #[cfg(test)]
    pub(crate) fn variant_targets(&self, id: ResourceId) -> Vec<DeviceSize> {
        self.entries
            .get(id)
            .map(|entry| {
                entry
                    .variants
                    .iter()
                    .map(|variant| variant.target)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn variant_pixels(
        &self,
        image: ResourceImage,
        target: DeviceSize,
    ) -> Option<(u32, u32, Vec<u8>)> {
        let entry = self.entries.get(image.id)?;
        let index = select_variant(entry, target)?;
        let variant = entry.variants.get(index)?;
        let width = u32::try_from(variant.target.width.value()).ok()?;
        let height = u32::try_from(variant.target.height.value()).ok()?;
        let ((page_width, _), pixels) = self.page_pixels_at(variant.tile.page)?;
        let origin_x = (variant.tile.u0 * page_width as f32).round() as usize;
        let origin_y = (variant.tile.v0 * page_width as f32).round() as usize;
        let mut out = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height as usize {
            let start = (origin_y + y) * page_width as usize * 4 + origin_x * 4;
            out.extend_from_slice(pixels.get(start..start + width as usize * 4)?);
        }
        Some((width, height, out))
    }

    fn push_pending(&mut self, job: PendingJob) -> PendingResource {
        let id = self.pending.insert_with(|_| job);
        self.pending_ids.push(id);
        id
    }

    fn push_failed(&mut self, cause: DecodeCause, owner: Option<WindowId>) -> PendingResource {
        self.push_pending(PendingJob::settled(DecodeState::Failed(cause), owner))
    }

    fn push_ready(&mut self, image: ResourceImage, owner: Option<WindowId>) -> PendingResource {
        self.push_pending(PendingJob::settled(DecodeState::Ready(image), owner))
    }

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

    fn finish_bitmap(
        &mut self,
        source: Bytes,
        decoded: DecodedImage,
        key: ContentKey,
        owner: Option<WindowId>,
    ) -> DecodeState {
        if let Some(image) = self.dedupe_hit(&key, &source, owner) {
            return DecodeState::Ready(image);
        }
        match self.commit_bitmap(source, decoded, key, owner) {
            Ok(image) => DecodeState::Ready(image),
            Err(cause) => DecodeState::Failed(cause),
        }
    }

    fn commit_bitmap(
        &mut self,
        source: Bytes,
        decoded: DecodedImage,
        key: ContentKey,
        owner: Option<WindowId>,
    ) -> Result<ResourceImage, DecodeCause> {
        if let Some(image) = self.dedupe_hit(&key, &source, owner) {
            return Ok(image);
        }
        let native = DeviceSize::new(dp(decoded.width as i32), dp(decoded.height as i32));
        let mut variants: SmallVec<[Variant; 3]> = SmallVec::new();
        variants.push(self.place_variant(&decoded, native, true)?);
        let lod_limit = {
            #[cfg(test)]
            {
                usize::from(self.lod_levels)
            }
            #[cfg(not(test))]
            {
                2usize
            }
        };
        for lod in lod_sizes(decoded.width, decoded.height)
            .into_iter()
            .take(lod_limit)
        {
            if self.pager.would_open_new_page(lod) {
                continue;
            }
            let scaled = image::downsample(&decoded, lod.0, lod.1);
            let target = DeviceSize::new(dp(lod.0 as i32), dp(lod.1 as i32));
            if let Ok(variant) = self.place_variant(&scaled, target, false) {
                variants.push(variant);
            }
        }
        let kind = ImageResourceKind::Bitmap { native };
        self.note_color(decoded.color);
        let epoch = self.next_epoch();
        let image = self.insert_resource_with_variants(source, kind, key, variants, epoch);
        if let Some(owner) = owner {
            self.register_owner(owner, image.id);
        }
        Ok(image)
    }

    fn insert_resource(
        &mut self,
        source: Bytes,
        kind: ImageResourceKind,
        key: ContentKey,
        owner: Option<WindowId>,
    ) -> ResourceImage {
        let epoch = self.next_epoch();
        let id = self.entries.insert_with(|_| Entry {
            kind,
            key,
            variants: SmallVec::new(),
            refcount: 1,
            last_used: epoch,
        });
        self.register_content(key, source, id);
        if let Some(owner) = owner {
            self.register_owner(owner, id);
        }
        ResourceImage { id, kind }
    }

    fn insert_resource_with_variants(
        &mut self,
        source: Bytes,
        kind: ImageResourceKind,
        key: ContentKey,
        variants: SmallVec<[Variant; 3]>,
        epoch: u64,
    ) -> ResourceImage {
        let variant_count = variants.len();
        let id = self.entries.insert_with(|_| Entry {
            kind,
            key,
            variants,
            refcount: 1,
            last_used: epoch,
        });
        self.variant_count = self.variant_count.saturating_add(variant_count);
        self.register_content(key, source, id);
        ResourceImage { id, kind }
    }

    fn register_content(&mut self, key: ContentKey, source: Bytes, id: ResourceId) {
        self.source_bytes = self.source_bytes.saturating_add(source.len() as u64);
        if let Some(replaced) = self.dedupe.insert(key, DedupeEntry { bytes: source, id }) {
            self.source_bytes = self
                .source_bytes
                .saturating_sub(replaced.bytes.len() as u64);
        }
    }

    fn register_owner(&mut self, owner: WindowId, id: ResourceId) {
        let owned = self.owners.entry(owner).or_default();
        if !owned.contains(&id) {
            owned.push(id);
        }
    }

    fn note_color(&mut self, color: SourceColor) {
        match color {
            SourceColor::Srgb => {}
            SourceColor::Converted => {
                self.color_converted = self.color_converted.saturating_add(1);
            }
            SourceColor::AssumedSrgb => {
                self.color_fallback = self.color_fallback.saturating_add(1);
            }
        }
    }

    fn dedupe_hit(
        &mut self,
        key: &ContentKey,
        bytes: &[u8],
        owner: Option<WindowId>,
    ) -> Option<ResourceImage> {
        let existing = self.dedupe.get(key)?;
        if existing.bytes.as_ref() != bytes {
            return None;
        }
        let id = existing.id;
        if let Some(owner) = owner
            && self
                .owners
                .get(&owner)
                .is_some_and(|owned| owned.contains(&id))
        {
            return self.entries.get(id).map(|entry| ResourceImage {
                id,
                kind: entry.kind,
            });
        }
        let epoch = self.next_epoch();
        let kind = {
            let Some(entry) = self.entries.get_mut(id) else {
                if let Some(removed) = self.dedupe.remove(key) {
                    self.source_bytes =
                        self.source_bytes.saturating_sub(removed.bytes.len() as u64);
                }
                return None;
            };
            entry.refcount = entry.refcount.saturating_add(1);
            entry.last_used = epoch;
            entry.kind
        };
        if let Some(owner) = owner {
            self.register_owner(owner, id);
        }
        Some(ResourceImage { id, kind })
    }

    fn admit_decode(
        &mut self,
        width: u32,
        height: u32,
        source_len: usize,
    ) -> Result<u64, DecodeCause> {
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

    fn release_reservation(&mut self, reservation: u64) {
        self.reserved_decode_bytes = self.reserved_decode_bytes.saturating_sub(reservation);
    }

    fn place_variant(
        &mut self,
        decoded: &DecodedImage,
        target: DeviceSize,
        count_capacity: bool,
    ) -> Result<Variant, DecodeCause> {
        let (width, height) = (decoded.width, decoded.height);
        let Some(allocation) = self.pager.allocate((width, height)) else {
            if count_capacity {
                self.capacity_errors = self.capacity_errors.saturating_add(1);
            }
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
        buffer.clear_rect(allocation.padded);
        buffer.write(allocation.content, &decoded.bgra, width as usize * 4);
        self.pager.mark_written(page, allocation.padded);
        let pixels = u64::from(width) * u64::from(height);
        self.used_pixels = self.used_pixels.saturating_add(pixels);
        debug_assert!(
            self.used_bytes().saturating_sub(self.reserved_decode_bytes) <= self.byte_budget,
            "RD-2 预算不变量被破坏：used={} budget={}",
            self.used_bytes(),
            self.byte_budget
        );
        Ok(Variant {
            target,
            pixels,
            tile: allocation.tile,
            alloc: allocation.id,
            padded: allocation.padded,
            last_used: self.next_epoch(),
            pinned_frame: self.frame_epoch,
            exposed: false,
        })
    }

    pub(crate) fn unload(&mut self, id: ResourceId) -> bool {
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

    pub(crate) fn release_window(&mut self, owner: WindowId) {
        for id in self
            .pending_ids
            .iter()
            .copied()
            .filter(|pending| {
                self.pending
                    .get(*pending)
                    .is_some_and(|job| job.owner == Some(owner))
            })
            .collect::<Vec<_>>()
        {
            if let Some(job) = self.pending.remove(id) {
                self.release_reservation(job.reservation);
            }
        }
        let Some(mut ids) = self.owners.remove(&owner) else {
            return;
        };
        ids.sort_by_key(|id| self.entries.get(*id).map_or(0, |entry| entry.last_used));
        for id in ids {
            self.unload(id);
        }
    }

    #[must_use]
    pub fn stats(&self) -> ResourceStats {
        let (page_width, page_height) = self.page_dimensions();
        let page_pixels = u64::from(page_width) * u64::from(page_height);
        let pages = self.pager.page_count();
        let entries = self.entries.len();
        ResourceStats {
            entries,
            tiles: self.variant_count,
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

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn page_count(&self) -> usize {
        self.pager.page_count()
    }

    pub(crate) fn page_extent(&self) -> u32 {
        self.page_dimensions().0
    }

    pub(crate) fn page_pixels_at(&self, page: u16) -> Option<((u32, u32), &[u8])> {
        let buffer = self.page_buffers.get(usize::from(page))?.as_ref()?;
        Some((buffer.dimensions(), buffer.bytes()))
    }

    #[cfg(test)]
    pub(crate) fn page_pixels(&self) -> Option<((u32, u32), &[u8])> {
        self.page_pixels_at(0)
    }

    pub(crate) fn take_dirty_rects_at(&mut self, page: u16) -> Option<DirtyRects> {
        self.pager.take_dirty_rects(page)
    }

    #[cfg(test)]
    pub(crate) fn take_dirty_rects(&mut self) -> DirtyRects {
        self.take_dirty_rects_at(0).unwrap_or_default()
    }

    fn reclaim(&mut self, id: ResourceId) {
        let Some(entry) = self.entries.remove(id) else {
            return;
        };
        for variant in &entry.variants {
            self.used_pixels = self.used_pixels.saturating_sub(variant.pixels);
            let page = variant.tile.page;
            if let Some(buffer) = self
                .page_buffers
                .get_mut(usize::from(page))
                .and_then(Option::as_mut)
            {
                buffer.clear_rect(variant.padded);
            }
            self.pager.deallocate(page, variant.alloc);
            self.pager.mark_written(page, variant.padded);
        }
        self.variant_count = self.variant_count.saturating_sub(entry.variants.len());
        if self
            .dedupe
            .get(&entry.key)
            .is_some_and(|existing| existing.id == id)
            && let Some(removed) = self.dedupe.remove(&entry.key)
        {
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
    use std::{mem::size_of, sync::atomic::Ordering, time::Duration};

    use bytes::Bytes;

    use super::{
        CapacityCause, DEFAULT_BYTE_BUDGET, DecodeCause, DecodeState, ImageDiagnostic,
        MAX_CONCURRENT_DECODES, PendingResource, ResourceCache, ResourceId, ResourceImage,
        ResourceStats, VariantState,
    };
    use crate::{
        error::EngineError,
        math::{DevicePixelRatio, DeviceRect, DeviceSize, LogicalSize, dp, px},
        paint::atlas::{AtlasKind, MAX_IMAGE_PAGES},
        platform::dispatcher::Dispatcher,
        resources::{
            ImageResourceKind,
            image::{MAX_SOURCE_BYTES, MAX_SOURCE_DIM, MAX_SOURCE_PIXELS},
        },
        window::WindowId,
    };

    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let pixels: Vec<u8> = std::iter::repeat_n(rgba, (width * height) as usize)
            .flatten()
            .collect();
        rgba_png(width, height, &pixels)
    }

    fn rgba_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let image = image::RgbaImage::from_raw(width, height, pixels.to_vec()).expect("raw");
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode");
        out.into_inner()
    }

    fn svg(side: f32) -> Vec<u8> {
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{side}\" height=\"{side}\" \
             viewBox=\"0 0 {side} {side}\"><path d=\"M2 2H14V14H2Z\" fill=\"#FFFFFF\"/></svg>"
        )
        .into_bytes()
    }

    fn cache() -> ResourceCache {
        ResourceCache::with_page_for_test((16, 16))
    }

    fn svg_cache() -> ResourceCache {
        ResourceCache::with_page_for_test((128, 128))
    }

    fn inserted(cache: &mut ResourceCache, rgba: [u8; 4]) -> ResourceImage {
        cache.insert(&png(4, 4, rgba)).expect("insert 4x4 png")
    }

    fn decode_cause(error: EngineError) -> DecodeCause {
        match error {
            EngineError::ResourceDecode { cause } => cause,
            other => panic!("expected ResourceDecode, got {other:?}"),
        }
    }

    fn intrinsic(side: f32) -> LogicalSize {
        LogicalSize::new(px(side), px(side))
    }

    fn target(side: i32) -> DeviceSize {
        DeviceSize::new(dp(side), dp(side))
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    fn ensure(
        cache: &mut ResourceCache,
        image: ResourceImage,
        physical: DeviceSize,
    ) -> VariantState {
        cache
            .ensure_variant(image, physical)
            .expect("资源必须仍然存活")
    }

    fn created(state: VariantState) -> bool {
        matches!(state, VariantState::Ready { created: true })
    }

    fn failed_cause(state: VariantState) -> Option<DecodeCause> {
        match state {
            VariantState::Ready { .. } => None,
            VariantState::Failed { cause } => Some(cause),
        }
    }

    fn managed(runtime: &tokio::runtime::Runtime) -> ResourceCache {
        ResourceCache::new(runtime.handle().clone(), Dispatcher::new())
    }

    fn poll_until(cache: &mut ResourceCache, pending: PendingResource) -> DecodeState {
        for _ in 0..2000 {
            for (handle, state) in cache.poll() {
                if handle == pending {
                    return state;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("async decode did not finish within 2000 ms");
    }

    fn tile_pixel(cache: &ResourceCache, image: ResourceImage) -> [u8; 4] {
        let tile = image
            .variant_for(cache, target(4096))
            .expect("位图的原生变体恒可用");
        let ((page_width, _), pixels) = cache.page_pixels().expect("image page");
        let origin_x = (tile.u0 * page_width as f32).round() as usize;
        let origin_y = (tile.v0 * page_width as f32).round() as usize;
        let offset = origin_y * page_width as usize * 4 + origin_x * 4;
        pixels[offset..offset + 4].try_into().expect("4 字节像素")
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

    #[test]
    fn insert_dedupes_identical_bytes() {
        let mut cache = large_cache();
        let bytes = png(4, 4, [1, 2, 3, 255]);
        let first = cache.insert(&bytes).expect("first insert");
        let second = cache
            .insert(&bytes)
            .expect("second insert hits the content index");

        assert_eq!(first, second, "同一份字节必须复用同一资源句柄");
        assert_eq!(cache.len(), 1, "去重后只有一条资源");
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().tiles, 3, "原生 + 1/2 + 1/4 三个变体");
        assert_eq!(
            cache.entries.get(first.id()).expect("entry").refcount,
            2,
            "两次插入 = 引用计数 2"
        );

        assert!(cache.unload(first.id()));
        assert_eq!(cache.len(), 1, "引用计数未归零，不得回收 tile");
        assert!(cache.unload(second.id()));
        assert_eq!(cache.len(), 0, "引用计数归零才回收");
        assert!(cache.dedupe.is_empty(), "回收后内容索引必须同步清掉");
        assert_eq!(cache.stats().used_pixels, 0);
        assert_eq!(cache.stats().used_bytes, 512 * 512 * 4, "只剩图片页本身");
    }

    #[test]
    fn insert_png_registers_the_native_pixels() {
        let mut cache = ResourceCache::new_unmanaged();
        let image = inserted(&mut cache, [10, 20, 30, 128]);

        assert_eq!(
            image.kind(),
            ImageResourceKind::Bitmap { native: target(4) },
            "位图登记原生物理像素"
        );
        assert_eq!(image.intrinsic(DevicePixelRatio::ONE), intrinsic(4.0));
        let tile = image.variant_for(&cache, target(4)).expect("native tile");
        assert_eq!(tile.atlas, AtlasKind::Image);
        assert_eq!(tile.generation, 1);
        assert!(tile.u0 < tile.u1 && tile.v0 < tile.v1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn bitmap_insert_keeps_the_native_pixels_and_builds_half_and_quarter_lods() {
        let mut cache = large_cache();
        let image = inserted(&mut cache, [1, 2, 3, 255]);
        assert_eq!(
            cache.variant_targets(image.id()),
            vec![target(4), target(2), target(1)],
            "位图变体 = 原生 + 1/2 + 1/4（解码期 Triangle 预缩）"
        );
        assert_eq!(
            cache.ensure_variant(image, target(4)),
            Some(VariantState::Ready { created: false })
        );
        assert_eq!(
            cache.ensure_variant(image, target(2)),
            Some(VariantState::Ready { created: false })
        );
        let native = image.variant_for(&cache, target(64)).expect("native tile");
        let half = image.variant_for(&cache, target(2)).expect("half tile");
        let quarter = image.variant_for(&cache, target(1)).expect("quarter tile");
        assert_ne!(native, half);
        assert_ne!(half, quarter);
        assert_eq!(
            image.variant_for(&cache, target(4)),
            Some(native),
            "目标 ≥ 原生时用原生"
        );
        assert_eq!(cache.stats().tiles, 3);
        assert_eq!(cache.stats().used_pixels, 16 + 4 + 1);
    }

    #[test]
    fn bitmap_intrinsic_follows_the_window_dpr() {
        let mut cache = cache();
        let image = inserted(&mut cache, [1, 2, 3, 255]);
        assert_eq!(
            image.intrinsic(DevicePixelRatio::new(2.0).expect("scale")),
            LogicalSize::new(px(2.0), px(2.0)),
            "位图内在逻辑尺寸 = 原生物理像素 ÷ DPR"
        );
        assert_eq!(
            image.kind(),
            ImageResourceKind::Bitmap { native: target(4) },
            "DPI 不改变原生像素"
        );
    }

    #[test]
    fn insert_rejects_unsupported_and_unknown_formats() {
        let mut cache = cache();
        assert_eq!(
            decode_cause(
                cache
                    .insert(b"GIF89a\x01\x00\x01\x00\x00\x00\x00")
                    .expect_err("gif")
            ),
            DecodeCause::UnsupportedFormat {
                format: smol_str::SmolStr::new_static("gif")
            }
        );
        assert_eq!(
            decode_cause(cache.insert(&[0u8; 8]).expect_err("unknown")),
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
        let error = decode_cause(cache.insert(&png[..png.len() / 2]).expect_err("truncated"));
        assert!(
            matches!(&error, DecodeCause::Corrupt { format, .. } if format == "png"),
            "{error:?}"
        );
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn insert_reports_source_size_limits() {
        let mut cache = cache();
        let wide = png(MAX_SOURCE_DIM + 1, 1, [1, 2, 3, 255]);
        assert_eq!(
            decode_cause(cache.insert(&wide).expect_err("too large")),
            DecodeCause::TooLarge {
                width: MAX_SOURCE_DIM + 1,
                height: 1,
                pixels: u64::from(MAX_SOURCE_DIM) + 1,
                limit: MAX_SOURCE_PIXELS,
                dim_limit: MAX_SOURCE_DIM,
            }
        );
        assert_eq!(cache.stats().capacity_errors, 0, "尺寸失败不是容量失败");
    }

    #[test]
    fn svg_registers_the_intrinsic_without_rasterizing() {
        let mut cache = svg_cache();
        let image = cache.insert_svg(&svg(16.0), intrinsic(16.0)).expect("svg");
        assert_eq!(
            image.kind(),
            ImageResourceKind::Vector {
                intrinsic: intrinsic(16.0)
            }
        );
        assert_eq!(image.intrinsic(DevicePixelRatio::ONE), intrinsic(16.0));
        assert_eq!(
            image.intrinsic(DevicePixelRatio::new(3.0).expect("scale")),
            intrinsic(16.0),
            "SVG 内在逻辑尺寸与 DPR 无关"
        );
        assert!(cache.variant_targets(image.id()).is_empty());
        assert_eq!(cache.stats().tiles, 0);
        assert_eq!(cache.stats().used_pixels, 0);
        assert_eq!(image.variant_for(&cache, target(16)), None);
    }

    #[test]
    fn svg_ensure_variant_generates_quantized_targets_once() {
        let mut cache = svg_cache();
        let image = cache.insert_svg(&svg(16.0), intrinsic(16.0)).expect("svg");
        assert!(created(ensure(&mut cache, image, target(16))));
        assert!(
            !created(ensure(&mut cache, image, target(16))),
            "同一目标必须复用"
        );
        assert!(
            created(ensure(&mut cache, image, target(17))),
            "17 物理像素量化到 20"
        );
        assert!(
            !created(ensure(&mut cache, image, target(18))),
            "18 物理像素同样量化到 20，必须复用"
        );
        assert_eq!(
            cache.variant_targets(image.id()),
            vec![target(16), target(20)]
        );
        let small = image.variant_for(&cache, target(16)).expect("16 tile");
        let large = image.variant_for(&cache, target(20)).expect("20 tile");
        assert_ne!(small, large, "不同物理尺寸必须是不同栅格");
        assert_eq!(cache.stats().tiles, 2);
        assert_eq!(cache.stats().used_pixels, 16 * 16 + 20 * 20);
    }

    #[test]
    fn svg_variants_cover_fractional_dprs() {
        let mut cache = svg_cache();
        let image = cache.insert_svg(&svg(16.0), intrinsic(16.0)).expect("svg");
        for (factor, physical) in [(1.0f32, 16i32), (1.5, 24), (2.0, 32)] {
            let side = (16.0 * factor).ceil() as i32;
            assert!(
                created(ensure(&mut cache, image, target(side))),
                "{factor}× 的物理目标 {physical} 必须首次生成"
            );
        }
        assert_eq!(
            cache.variant_targets(image.id()),
            vec![target(16), target(24), target(32)]
        );
        let tiles: Vec<_> = [16, 24, 32]
            .into_iter()
            .map(|side| image.variant_for(&cache, target(side)).expect("tile"))
            .collect();
        assert_ne!(tiles[0], tiles[1]);
        assert_ne!(tiles[1], tiles[2]);
        assert_ne!(tiles[0], tiles[2]);
    }

    #[test]
    fn svg_variants_can_upscale_above_the_intrinsic_size() {
        let mut cache = svg_cache();
        let image = cache.insert_svg(&svg(16.0), intrinsic(16.0)).expect("svg");
        assert!(created(ensure(&mut cache, image, target(64))));
        assert_eq!(cache.variant_targets(image.id()), vec![target(64)]);
        assert_eq!(cache.stats().used_pixels, 64 * 64);
    }

    #[test]
    fn svg_dedupe_includes_the_intrinsic_size() {
        let mut cache = svg_cache();
        let bytes = svg(16.0);
        let square = cache.insert_svg(&bytes, intrinsic(16.0)).expect("square");
        let again = cache.insert_svg(&bytes, intrinsic(16.0)).expect("same");
        let wide = cache
            .insert_svg(&bytes, LogicalSize::new(px(32.0), px(16.0)))
            .expect("wide");
        assert_eq!(square.id(), again.id(), "同字节同 intrinsic 必须去重");
        assert_ne!(
            square.id(),
            wide.id(),
            "同一份 SVG 字节在不同内在尺寸下是不同资源"
        );
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn svg_insert_rejects_invalid_intrinsic_and_corrupt_bytes() {
        let mut cache = svg_cache();
        let error = cache
            .insert_svg(&svg(16.0), LogicalSize::new(px(0.0), px(16.0)))
            .expect_err("零内在尺寸");
        assert!(
            matches!(error, EngineError::InvalidImageIntrinsic { .. }),
            "{error:?}"
        );
        let error = cache
            .insert_svg(b"<svg", intrinsic(8.0))
            .expect_err("坏字节");
        assert!(
            matches!(
                error,
                EngineError::ResourceDecode {
                    cause: DecodeCause::Corrupt { ref format, .. }
                } if format == "svg"
            ),
            "{error:?}"
        );
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn svg_variants_are_reclaimed_with_the_resource() {
        let mut cache = svg_cache();
        let image = cache.insert_svg(&svg(16.0), intrinsic(16.0)).expect("svg");
        assert!(created(ensure(&mut cache, image, target(16))));
        assert!(created(ensure(&mut cache, image, target(32))));
        assert_eq!(cache.stats().used_pixels, 16 * 16 + 32 * 32);
        assert!(cache.unload(image.id()));
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.stats().tiles, 0);
        assert_eq!(cache.stats().used_pixels, 0);
        assert_eq!(cache.stats().used_bytes, 128 * 128 * 4, "回到页常驻");
        assert!(cache.dedupe.is_empty());
    }

    #[test]
    fn page_capacity_is_independent_of_insertion_order() {
        let mut cache = ResourceCache::new_unmanaged();
        let tiny = inserted(&mut cache, [1, 2, 3, 255]);
        assert_eq!(tiny.kind(), ImageResourceKind::Bitmap { native: target(4) });
        let big = cache
            .insert(&png(600, 600, [9, 9, 9, 255]))
            .expect("600x600 must fit after a tiny image");
        assert!(matches!(
            big.kind(),
            ImageResourceKind::Bitmap { native } if native == DeviceSize::new(dp(600), dp(600))
        ));
        assert_eq!(cache.len(), 2);
    }

    fn fill_until_capacity(cache: &mut ResourceCache) -> (Vec<ResourceImage>, DecodeCause) {
        let mut images = Vec::new();
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
                    value = value.wrapping_add(1);
                }
                Err(error) => return (images, decode_cause(error)),
            }
        }
    }

    #[test]
    fn insert_reports_capacity_with_sizes() {
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let (images, error) = fill_until_capacity(&mut cache);
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
        assert_eq!(cache.stats().capacity_errors, 1, "容量失败要计数");
    }

    #[test]
    fn image_atlas_grows_to_multiple_pages() {
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let (images, _error) = fill_until_capacity(&mut cache);
        let mut pages: Vec<u16> = images
            .iter()
            .map(|image| {
                image
                    .variant_for(&cache, target(1))
                    .expect("native tile")
                    .page
            })
            .collect();
        pages.sort_unstable();
        pages.dedup();
        assert_eq!(pages, (0..MAX_IMAGE_PAGES).collect::<Vec<_>>());
        let stats = cache.stats();
        assert_eq!(stats.pages, usize::from(MAX_IMAGE_PAGES));
        assert_eq!(stats.tiles, images.len());
    }

    #[test]
    fn nine_image_tiles_upload_a_bounding_box_not_the_whole_page() {
        let mut cache = ResourceCache::with_page_for_test((2048, 2048));
        let _ = cache.take_dirty_rects();
        for index in 0..9i32 {
            let x = (index % 3) * 600;
            let y = (index / 3) * 600;
            cache
                .pager
                .mark_written(0, DeviceRect::new(dp(x), dp(y), dp(x + 34), dp(y + 34)));
        }
        let rects = cache.take_dirty_rects();
        assert!(!rects.is_full(), "9 块图片不得折叠成整页哨兵");
        let page_bytes = 2048u64 * 2048 * 4;
        let uploaded = rects.byte_count(2048, 4) as u64;
        assert!(
            uploaded < page_bytes,
            "上传字节 {uploaded} 必须小于整页 {page_bytes}"
        );
        assert_eq!(rects.chunks(2048, 4).count(), 1, "折叠结果是单个 bbox 矩形");
    }

    #[test]
    fn insert_marks_only_the_allocated_rect_dirty() {
        let mut cache = ResourceCache::with_page_for_test((256, 256));
        let _ = cache.take_dirty_rects();
        let _ = inserted(&mut cache, [1, 2, 3, 255]);
        let rects = cache.take_dirty_rects();
        assert!(!rects.is_full(), "插入 4x4 不得把整页标记为脏（只传 AOI）");
        assert!(rects.byte_count(256, 4) < 256 * 256 * 4);
    }

    #[test]
    fn page_pixels_hold_premultiplied_bgra() {
        let mut cache = ResourceCache::new_unmanaged();
        let image = inserted(&mut cache, [200, 100, 50, 255]);
        assert_eq!(
            tile_pixel(&cache, image),
            [50, 100, 200, 255],
            "照片像素必须按 BGRA + 线性空间预乘写入图集"
        );
    }

    #[test]
    fn unload_is_idempotent_and_stale_handles_stay_dead() {
        let mut cache = cache();
        let image = inserted(&mut cache, [1, 2, 3, 255]);
        assert!(cache.unload(image.id()));
        assert!(!cache.unload(image.id()), "已回收的句柄再卸载返回 false");
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.image(image.id()), None, "陈旧句柄不得复活");
        assert_eq!(cache.stats().used_pixels, 0);
    }

    #[test]
    fn unload_clears_the_page_region_and_marks_rects_dirty() {
        let mut cache = cache();
        let image = inserted(&mut cache, [9, 9, 9, 255]);
        let _ = cache.take_dirty_rects();
        assert!(cache.unload(image.id()));
        let rects = cache.take_dirty_rects();
        assert!(!rects.is_empty(), "回收必须把旧区域标记为脏（清成透明）");
        let ((_, _), pixels) = cache.page_pixels().expect("page");
        assert!(
            pixels.iter().all(|byte| *byte == 0),
            "未复用的图集区域必须清零"
        );
    }

    #[test]
    fn release_window_reclaims_only_that_windows_resources() {
        let mut cache = cache();
        let left = WindowId::for_test(1);
        let right = WindowId::for_test(2);
        let logo = png(4, 4, [1, 2, 3, 255]);
        let avatar = png(4, 4, [4, 5, 6, 255]);
        let left_logo = cache.insert_owned(left, &logo).expect("left logo");
        let right_logo = cache.insert_owned(right, &logo).expect("right logo");
        let _right_avatar = cache.insert_owned(right, &avatar).expect("right avatar");
        assert_eq!(cache.len(), 2, "两种内容两条资源");
        assert_eq!(
            cache.entries.get(left_logo.id()).expect("entry").refcount,
            2,
            "同一份内容被两个窗口各自持有一份引用"
        );

        cache.release_window(left);
        assert_eq!(cache.len(), 2, "只释放左窗口的引用，不回收共享内容");
        assert_eq!(
            cache.entries.get(right_logo.id()).expect("entry").refcount,
            1,
            "右窗口的引用仍在"
        );
        cache.release_window(left);
        assert_eq!(cache.len(), 2, "重复释放不得动其它窗口");
        cache.release_window(right);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn stats_track_page_usage_and_entries() {
        let mut cache = large_cache();
        let page_bytes = 512 * 512 * 4;
        assert_eq!(
            cache.stats(),
            ResourceStats {
                entries: 0,
                tiles: 0,
                pages: 1,
                used_pixels: 0,
                used_bytes: page_bytes,
                page_occupancy: 0.0,
                capacity_errors: 0,
                color_converted: 0,
                color_fallback: 0,
            },
            "预开页已经常驻"
        );
        let image = inserted(&mut cache, [1, 2, 3, 255]);
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.tiles, 3);
        assert_eq!(stats.used_pixels, 16 + 4 + 1);
        assert!(stats.used_bytes > page_bytes, "源字节进预算");
        assert!((stats.page_occupancy - 21.0 / (512.0 * 512.0)).abs() < 1e-9);
        assert!(cache.unload(image.id()));
        assert_eq!(cache.stats().used_pixels, 0);
        assert_eq!(cache.stats().used_bytes, page_bytes);
    }

    #[test]
    fn async_decode_reaches_ready_after_poll() {
        let runtime = runtime();
        let mut cache = managed(&runtime);
        let pending = cache.insert_async(Bytes::from(png(8, 8, [1, 2, 3, 255])));
        let state = poll_until(&mut cache, pending);
        let DecodeState::Ready(image) = state else {
            panic!("expected Ready, got {state:?}");
        };
        assert_eq!(
            image.kind(),
            ImageResourceKind::Bitmap { native: target(8) }
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.reserved_decode_bytes, 0, "交付后必须归还预留");
        assert_eq!(cache.poll().len(), 0, "poll 只交付一次");
    }

    #[test]
    fn insert_async_reports_corrupt_png_as_failed() {
        let runtime = runtime();
        let mut cache = managed(&runtime);
        let png = png(4, 4, [1, 2, 3, 255]);
        let pending = cache.insert_async(Bytes::from(png[..png.len() / 2].to_vec()));
        let state = poll_until(&mut cache, pending);
        assert!(
            matches!(&state, DecodeState::Failed(DecodeCause::Corrupt { format, .. }) if format == "png"),
            "{state:?}"
        );
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.reserved_decode_bytes, 0);
    }

    #[test]
    fn unmanaged_insert_async_is_ready_immediately() {
        let mut cache = ResourceCache::new_unmanaged();
        let pending = cache.insert_async(Bytes::from(png(4, 4, [9, 9, 9, 255])));
        let DecodeState::Ready(image) = cache.state(pending) else {
            panic!("unmanaged 模式必须同步解码并立即 Ready");
        };
        assert_eq!(
            image.kind(),
            ImageResourceKind::Bitmap { native: target(4) }
        );
        assert_eq!(cache.poll().len(), 1, "poll 交付一次");
        assert!(cache.poll().is_empty(), "交付后不再重复");
    }

    #[test]
    fn insert_async_hits_the_content_index_without_decoding() {
        let runtime = runtime();
        let mut cache = managed(&runtime);
        let bytes = png(4, 4, [1, 2, 3, 255]);
        let first = cache.insert_async(Bytes::from(bytes.clone()));
        assert!(matches!(
            poll_until(&mut cache, first),
            DecodeState::Ready(_)
        ));
        let starts = cache.decode_starts.load(Ordering::SeqCst);
        let second = cache.insert_async(Bytes::from(bytes));
        assert!(matches!(cache.state(second), DecodeState::Ready(_)));
        assert_eq!(
            cache.decode_starts.load(Ordering::SeqCst),
            starts,
            "内容索引命中不得再起解码"
        );
    }

    #[test]
    fn insert_async_rejects_oversize_source_bytes() {
        let mut cache = cache();
        let pending = cache.insert_async(Bytes::from(vec![0u8; MAX_SOURCE_BYTES + 1]));
        assert!(matches!(
            cache.state(pending),
            DecodeState::Failed(DecodeCause::SourceTooLarge { .. })
        ));
    }

    #[test]
    fn async_decode_worker_panic_delivers_failure_and_wakes() {
        let runtime = runtime();
        let mut cache = managed(&runtime);
        let _ = cache.dispatcher.take_frame_wake();
        cache.panic_decode_next.store(true, Ordering::SeqCst);
        let pending = cache.insert_async(Bytes::from(png(8, 8, [1, 2, 3, 255])));
        let state = poll_until(&mut cache, pending);
        assert!(matches!(
            state,
            DecodeState::Failed(DecodeCause::Corrupt { .. })
        ));
        assert!(cache.dispatcher.take_frame_wake(), "解码失败也必须唤醒泵");
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn async_decode_waits_for_a_free_concurrency_permit() {
        let runtime = runtime();
        let mut cache = managed(&runtime);
        let permits = cache
            .decode_semaphore
            .clone()
            .try_acquire_many_owned(u32::try_from(MAX_CONCURRENT_DECODES).expect("fits"))
            .expect("semaphore starts with full capacity");
        let pending = cache.insert_async(Bytes::from(png(4, 4, [1, 2, 3, 255])));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            cache.decode_starts.load(Ordering::SeqCst),
            0,
            "无许可不得起解码"
        );
        assert_eq!(cache.state(pending), DecodeState::Pending);
        drop(permits);
        assert!(matches!(
            poll_until(&mut cache, pending),
            DecodeState::Ready(_)
        ));
    }

    #[test]
    fn async_owners_are_released_with_the_window() {
        let runtime = runtime();
        let mut cache = managed(&runtime);
        let owner = WindowId::for_test(7);
        let pending = cache.insert_owned_async(owner, Bytes::from(png(8, 8, [1, 2, 3, 255])));
        assert!(matches!(
            poll_until(&mut cache, pending),
            DecodeState::Ready(_)
        ));
        assert_eq!(cache.len(), 1);
        cache.release_window(owner);
        assert_eq!(cache.len(), 0, "关窗必须回收异步交付的资源");
    }

    #[test]
    fn closing_a_window_drops_its_in_flight_decodes() {
        let runtime = runtime();
        let mut cache = managed(&runtime);
        let owner = WindowId::for_test(8);
        let pending = cache.insert_owned_async(owner, Bytes::from(png(64, 64, [1, 2, 3, 255])));
        cache.release_window(owner);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            cache.poll().is_empty(),
            "关窗后不得交付属于该窗口的异步结果"
        );
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.reserved_decode_bytes, 0);
        assert_eq!(
            cache.state(pending),
            DecodeState::Pending,
            "已取消的句柄保持哑状态"
        );
    }

    #[test]
    fn byte_budget_reports_capacity_and_recovers_after_unload() {
        let page_bytes = 64 * 64 * 4;
        let budget = 16 * 1024 * 1024 + page_bytes + 4096;
        let mut cache = ResourceCache::for_test((64, 64), budget, None);
        let mut images = Vec::new();
        let mut value = 1u8;
        let error = loop {
            let bytes = png(4, 4, [value, value.wrapping_add(1), 0, 255]);
            match cache.insert(&bytes) {
                Ok(image) => {
                    images.push(image);
                    value = value.wrapping_add(2);
                }
                Err(error) => break decode_cause(error),
            }
            assert!(images.len() < 512, "预算上限必须真的生效");
        };
        assert!(
            matches!(
                error,
                DecodeCause::Capacity {
                    cause: CapacityCause::ByteBudget { .. },
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(cache.stats().capacity_errors, 1);
        assert!(cache.stats().used_bytes <= budget);
        for image in images {
            assert!(cache.unload(image.id()));
        }
        assert_eq!(cache.len(), 0);
        assert!(cache.stats().used_bytes <= budget, "回收后必须回到预算内");
        let bytes = png(4, 4, [9, 9, 9, 255]);
        assert!(cache.insert(&bytes).is_ok(), "回收后必须能再次插入");
    }

    #[test]
    fn svg_async_owned_insert_delivers_immediately_and_is_released() {
        let runtime = runtime();
        let mut cache = managed(&runtime);
        let owner = WindowId::for_test(9);
        let pending = cache
            .insert_owned_svg_async(owner, Bytes::from(svg(16.0)), intrinsic(16.0))
            .expect("svg");
        let state = poll_until(&mut cache, pending);
        let DecodeState::Ready(image) = state else {
            panic!("expected Ready, got {state:?}");
        };
        assert_eq!(image.intrinsic(DevicePixelRatio::ONE), intrinsic(16.0));
        assert_eq!(cache.len(), 1);
        cache.release_window(owner);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn resource_stats_default_is_empty() {
        let stats = ResourceStats::default();
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.tiles, 0);
        assert_eq!(stats.capacity_errors, 0);
        assert_eq!(DEFAULT_BYTE_BUDGET, 320 * 1024 * 1024);
    }

    fn large_cache() -> ResourceCache {
        ResourceCache::with_page_for_test((512, 512))
    }

    fn marker_svg() -> Vec<u8> {
        b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\" \
          viewBox=\"0 0 100 100\"><rect width=\"100\" height=\"100\" fill=\"#FFFFFF\"/>\
          <rect width=\"20\" height=\"20\" fill=\"#FF0000\"/>\
          <rect x=\"80\" y=\"80\" width=\"20\" height=\"20\" fill=\"#0000FF\"/></svg>"
            .to_vec()
    }

    fn pixel_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let offset = (y as usize * width as usize + x as usize) * 4;
        pixels[offset..offset + 4].try_into().expect("4 字节像素")
    }

    const RED: [u8; 4] = [0, 0, 255, 255];
    const BLUE: [u8; 4] = [255, 0, 0, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    fn color_png(width: u32, height: u32, color_at: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&color_at(x, y));
            }
        }
        rgba_png(width, height, &pixels)
    }

    fn png_with_icc(width: u32, height: u32, rgba: [u8; 4], icc: &[u8]) -> Vec<u8> {
        use image::{ExtendedColorType, ImageEncoder};
        let pixels: Vec<u8> = std::iter::repeat_n(rgba, (width * height) as usize)
            .flatten()
            .collect();
        let mut out = std::io::Cursor::new(Vec::new());
        let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
        encoder
            .set_icc_profile(icc.to_vec())
            .expect("PngEncoder 支持 iCCP");
        encoder
            .write_image(&pixels, width, height, ExtendedColorType::Rgba8)
            .expect("encode png");
        out.into_inner()
    }

    fn display_p3_icc() -> Vec<u8> {
        moxcms::ColorProfile::new_display_p3()
            .encode()
            .expect("encode display p3 profile")
    }

    #[test]
    fn svg_rasterizes_the_whole_document_even_when_intrinsic_differs() {
        let mut cache = svg_cache();
        let image = cache
            .insert_svg(&marker_svg(), intrinsic(48.0))
            .expect("svg");
        assert_eq!(
            image.intrinsic(DevicePixelRatio::ONE),
            intrinsic(48.0),
            "intrinsic 只作为内在逻辑尺寸"
        );
        assert!(created(ensure(&mut cache, image, target(48))));
        let (width, height, pixels) = cache
            .variant_pixels(image, target(48))
            .expect("1× 48×48 变体");
        assert_eq!((width, height), (48, 48));
        assert_eq!(pixel_at(&pixels, width, 2, 2), RED, "左上角标记必须出现");
        assert_eq!(
            pixel_at(&pixels, width, 45, 45),
            BLUE,
            "右下角标记必须出现（文档尺寸 = 100，不能被 intrinsic 裁掉）"
        );
        assert_eq!(pixel_at(&pixels, width, 2, 45), WHITE);
        assert_eq!(pixel_at(&pixels, width, 45, 2), WHITE);

        assert!(created(ensure(&mut cache, image, target(72))));
        let (width, height, pixels) = cache
            .variant_pixels(image, target(72))
            .expect("1.5× 72×72 变体");
        assert_eq!((width, height), (72, 72));
        assert_eq!(pixel_at(&pixels, width, 3, 3), RED);
        assert_eq!(pixel_at(&pixels, width, 69, 69), BLUE);
    }

    #[test]
    fn svg_intrinsic_is_layout_only_and_document_size_defines_the_raster_mapping() {
        let mut cache = svg_cache();
        let intrinsic_size = LogicalSize::new(px(100.0), px(50.0));
        let image = cache
            .insert_svg(&marker_svg(), intrinsic_size)
            .expect("svg");
        assert_eq!(
            image.intrinsic(DevicePixelRatio::new(2.0).expect("scale")),
            intrinsic_size,
            "intrinsic 与 DPR 无关且不参与栅格化"
        );
        assert!(created(ensure(
            &mut cache,
            image,
            DeviceSize::new(dp(100), dp(50))
        )));
        let (width, height, pixels) = cache
            .variant_pixels(image, DeviceSize::new(dp(100), dp(50)))
            .expect("100×50 物理变体");
        assert_eq!((width, height), (100, 50));
        assert_eq!(
            pixel_at(&pixels, width, 90, 45),
            BLUE,
            "目标纵横比与文档不同时，文档被拉伸到目标盒（与 CSS width/height 一致）"
        );
        assert_eq!(pixel_at(&pixels, width, 5, 5), RED);
        assert_eq!(pixel_at(&pixels, width, 5, 45), WHITE);
    }

    #[test]
    fn svg_variant_failure_falls_back_to_the_nearest_smaller_variant() {
        let mut cache = ResourceCache::with_page_for_test((16, 16));
        let image = cache.insert_svg(&svg(16.0), intrinsic(16.0)).expect("svg");
        assert!(created(ensure(&mut cache, image, target(8))));
        let fallback = image.variant_for(&cache, target(8)).expect("8 tile");
        let cause = failed_cause(ensure(&mut cache, image, target(16))).expect("16 放不进 16 页");
        assert!(
            matches!(
                cause,
                DecodeCause::Capacity {
                    cause: CapacityCause::PageFull,
                    ..
                }
            ),
            "{cause:?}"
        );
        assert_eq!(
            image.variant_for(&cache, target(16)),
            Some(fallback),
            "生成失败必须回退到 ≤ 目标的最近变体，而不是让图片消失"
        );
        assert_eq!(cache.variant_targets(image.id()), vec![target(8)]);
    }

    #[test]
    fn svg_variants_are_capped_and_evicted_lru() {
        let mut cache = large_cache();
        let image = cache.insert_svg(&svg(16.0), intrinsic(16.0)).expect("svg");
        for frame in 0..12 {
            cache.begin_variant_frame();
            let side = 16 + frame * 4;
            assert!(
                created(ensure(&mut cache, image, target(side))),
                "{side} 物理像素变体必须生成"
            );
        }
        let targets = cache.variant_targets(image.id());
        assert_eq!(
            targets.len(),
            super::MAX_VARIANTS_PER_RESOURCE,
            "每资源变体数必须封顶：{targets:?}"
        );
        assert_eq!(
            targets,
            (4..12)
                .map(|frame| target(16 + frame * 4))
                .collect::<Vec<_>>(),
            "淘汰最久未使用的变体（保留最近 8 个）"
        );
        let expected: u64 = targets
            .iter()
            .map(|size| {
                u64::try_from(size.width.value()).expect("positive")
                    * u64::try_from(size.height.value()).expect("positive")
            })
            .sum();
        assert_eq!(cache.stats().used_pixels, expected, "淘汰必须回收像素预算");
        assert_eq!(cache.stats().tiles, super::MAX_VARIANTS_PER_RESOURCE);
    }

    #[test]
    fn svg_variant_admission_does_not_use_the_decoder_peak_estimate() {
        let page_bytes = 256 * 256 * 4;
        let budget = 8 * 1024 * 1024;
        let mut cache = ResourceCache::for_test((256, 256), budget, None);
        let image = cache.insert_svg(&svg(16.0), intrinsic(16.0)).expect("svg");
        assert!(
            created(ensure(&mut cache, image, target(200))),
            "200×200 变体的预留是 4 B/px + 小常量，不应被 16 MiB 解码峰值拒绝"
        );
        assert_eq!(cache.stats().used_pixels, 200 * 200);
        assert!(cache.stats().used_bytes <= budget);
        assert!(cache.stats().used_bytes >= page_bytes);
    }

    #[test]
    fn bitmap_half_lod_keeps_sharp_edges() {
        let mut cache = large_cache();
        let bytes = color_png(64, 64, |x, _| {
            if x < 32 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            }
        });
        let image = cache.insert(&bytes).expect("png");
        assert_eq!(
            cache.variant_targets(image.id()),
            vec![target(64), target(32), target(16)],
            "位图 = 原生 + 1/2 + 1/4"
        );
        let (width, _height, pixels) = cache.variant_pixels(image, target(32)).expect("1/2 LOD");
        assert_eq!(width, 32);
        assert_eq!(pixel_at(&pixels, width, 5, 16), RED, "左半内部仍是纯红");
        assert_eq!(pixel_at(&pixels, width, 26, 16), BLUE, "右半内部仍是纯蓝");
        let edge_left = pixel_at(&pixels, width, 15, 16);
        let edge_right = pixel_at(&pixels, width, 16, 16);
        assert!(
            edge_left[2] > 200 && edge_left[0] < 40,
            "1/2 LOD 的左边界列必须以红为主（单列过渡，不是大范围糊）：{edge_left:?}"
        );
        assert!(
            edge_right[0] > 200 && edge_right[2] < 40,
            "1/2 LOD 的右边界列必须以蓝为主：{edge_right:?}"
        );
        assert_eq!(
            image.variant_for(&cache, target(20)),
            image.variant_for(&cache, target(32)),
            "目标 20 选 ≥20 的最小变体（32）"
        );
        assert_eq!(
            image.variant_for(&cache, target(40)),
            image.variant_for(&cache, target(64)),
            "目标 40 选原生 64（没有 48 档）"
        );
    }

    #[test]
    fn repeated_owned_inserts_do_not_grow_the_owner_list() {
        let mut cache = cache();
        let owner = WindowId::for_test(11);
        let bytes = png(4, 4, [1, 2, 3, 255]);
        let first = cache.insert_owned(owner, &bytes).expect("first");
        for _ in 0..16 {
            let image = cache.insert_owned(owner, &bytes).expect("repeat");
            assert_eq!(image.id(), first.id(), "同一窗口同一内容复用同一资源");
        }
        assert_eq!(
            cache.owners.get(&owner).map(Vec::len),
            Some(1),
            "同一 (window, resource) 只登记一次 owner"
        );
        assert_eq!(
            cache.entries.get(first.id()).expect("entry").refcount,
            1,
            "重复插入不得累加引用计数"
        );
        cache.release_window(owner);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn color_conversion_is_counted_once_per_decoded_bitmap() {
        let mut cache = cache();
        let icc = display_p3_icc();
        let tagged = png_with_icc(2, 2, [234, 51, 35, 255], &icc);
        let image = cache.insert(&tagged).expect("p3 png");
        assert_eq!(cache.stats().color_converted, 1);
        assert_eq!(cache.stats().color_fallback, 0);
        assert!(matches!(
            image.kind(),
            ImageResourceKind::Bitmap { native } if native == target(2)
        ));
        let plain = png(2, 2, [10, 20, 30, 255]);
        let _ = cache.insert(&plain).expect("plain png");
        assert_eq!(cache.stats().color_converted, 1, "无需转换的位图不得计数");
    }

    #[test]
    fn invalid_icc_falls_back_and_is_counted() {
        let mut cache = cache();
        let tagged = png_with_icc(2, 2, [12, 34, 56, 128], b"not an icc profile");
        let image = cache.insert(&tagged).expect("bad icc still decodes");
        assert_eq!(cache.stats().color_fallback, 1);
        assert_eq!(cache.stats().color_converted, 0);
        let plain = png(2, 2, [12, 34, 56, 128]);
        let plain_image = cache.insert(&plain).expect("plain");
        let tagged_pixels = cache
            .variant_pixels(image, target(4096))
            .expect("native pixels");
        let plain_pixels = cache
            .variant_pixels(plain_image, target(4096))
            .expect("native pixels");
        assert_eq!(
            tagged_pixels.2, plain_pixels.2,
            "无效 ICC 降级必须与无 profile 路径逐字节一致"
        );
    }

    #[test]
    fn image_diagnostic_names_the_failed_resource() {
        let diagnostic = ImageDiagnostic::VariantUnavailable {
            resource: ResourceId::for_test(3, 2),
            width: 24,
            height: 24,
            cause: DecodeCause::Capacity {
                width: 24,
                height: 24,
                page: (16, 16),
                cause: CapacityCause::PageFull,
            },
        };
        assert_eq!(diagnostic.resource(), ResourceId::for_test(3, 2));
        assert!(format!("{diagnostic:?}").contains("VariantUnavailable"));
    }
}

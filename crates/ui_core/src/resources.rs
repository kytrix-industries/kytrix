use std::{fmt, num::NonZeroU32, sync::Arc};

use bytes::Bytes;
use smol_str::SmolStr;
use tokio::sync::{
    Semaphore,
    oneshot::{self, error::TryRecvError},
};

use crate::{
    error::EngineError,
    math::{DevicePixelRatio, LogicalPx, LogicalSize, PhysicalSize},
    node::id::{Arena, ArenaKey},
    paint::atlas::{AtlasKind, AtlasTile},
    platform::dispatcher::Dispatcher,
    render::{DirtyRects, ImageAtlasPager},
    resources::{
        cache::{ContentKey, DedupeEntry, Entry, Page, VariantEntry},
        image::{DecodedImage, MAX_TILE_DIM, SourceColor, SourceKind},
    },
    window::WindowId,
};

mod cache;
mod image;
mod svg;

const ORIGINAL_TARGET: (u32, u32) = (0, 0);

const DEFAULT_BYTE_BUDGET: u64 = 320 * 1024 * 1024;

const MAX_CONCURRENT_DECODES: usize = 2;

fn physical_target(logical: LogicalSize, scale: f32) -> PhysicalSize {
    DevicePixelRatio::new(scale)
        .unwrap_or(DevicePixelRatio::ONE)
        .to_physical_size(logical)
}

fn sane_logical(logical: LogicalSize) -> Option<LogicalSize> {
    (logical.width.is_finite()
        && logical.width > LogicalPx::ZERO
        && logical.height.is_finite()
        && logical.height > LogicalPx::ZERO)
        .then_some(logical)
}

fn probe_source(kind: SourceKind, bytes: &[u8]) -> Result<(u32, u32), DecodeCause> {
    match kind {
        SourceKind::Bitmap => image::probe_dimensions(bytes),
        SourceKind::Svg => svg::probe_dimensions(bytes),
    }
}

fn resolve_target(kind: SourceKind, target: PhysicalSize, source: (u32, u32)) -> (u32, u32) {
    match kind {
        SourceKind::Bitmap => image::resolve_target(target, source),
        SourceKind::Svg => svg::resolve_target(target, source),
    }
}

fn logical_target(target: LogicalSize) -> PhysicalSize {
    DevicePixelRatio::ONE.to_physical_size(target)
}

fn decode_source(
    kind: SourceKind,
    bytes: &[u8],
    target: Option<(u32, u32)>,
) -> Result<DecodedImage, DecodeCause> {
    match kind {
        SourceKind::Bitmap => image::decode(bytes, target),
        SourceKind::Svg => svg::decode(bytes, target),
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceImage {
    pub id: ResourceId,
    pub tile: AtlasTile,
    pub width: u32,
    pub height: u32,
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
    epoch: u64,
    byte_budget: u64,
    capacity_errors: u64,
    color_converted: u64,
    color_fallback: u64,
    #[cfg(test)]
    decode_starts: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    panic_decode_next: Arc<std::sync::atomic::AtomicBool>,
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
    target: (u32, u32),
    intrinsic: (u32, u32),
    kind: SourceKind,
    logical: Option<LogicalSize>,
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

    pub fn insert(&mut self, bytes: &[u8]) -> Result<ResourceImage, EngineError> {
        self.insert_inner(bytes, None, SourceKind::Bitmap, None)
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    pub fn insert_at_size(
        &mut self,
        bytes: &[u8],
        target: LogicalSize,
    ) -> Result<ResourceImage, EngineError> {
        self.insert_inner(
            bytes,
            Some(logical_target(target)),
            SourceKind::Bitmap,
            None,
        )
        .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    pub fn insert_for_display(
        &mut self,
        bytes: &[u8],
        logical: LogicalSize,
        scale: f32,
    ) -> Result<ResourceImage, EngineError> {
        self.insert_inner(
            bytes,
            Some(physical_target(logical, scale)),
            SourceKind::Bitmap,
            Some(logical),
        )
        .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    pub fn insert_svg(&mut self, bytes: &[u8]) -> Result<ResourceImage, EngineError> {
        self.insert_inner(bytes, None, SourceKind::Svg, None)
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    pub fn insert_svg_at_size(
        &mut self,
        bytes: &[u8],
        target: LogicalSize,
    ) -> Result<ResourceImage, EngineError> {
        self.insert_inner(bytes, Some(logical_target(target)), SourceKind::Svg, None)
            .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    pub fn insert_svg_for_display(
        &mut self,
        bytes: &[u8],
        logical: LogicalSize,
        scale: f32,
    ) -> Result<ResourceImage, EngineError> {
        self.insert_inner(
            bytes,
            Some(physical_target(logical, scale)),
            SourceKind::Svg,
            Some(logical),
        )
        .map_err(|cause| EngineError::ResourceDecode { cause })
    }

    fn insert_inner(
        &mut self,
        bytes: &[u8],
        target: Option<PhysicalSize>,
        kind: SourceKind,
        logical: Option<LogicalSize>,
    ) -> Result<ResourceImage, DecodeCause> {
        let logical = logical.and_then(sane_logical);
        image::check_source_bytes(bytes.len())?;
        if target.is_none()
            && let Some(image) = self.dedupe_hit(bytes, ORIGINAL_TARGET, kind)
        {
            return Ok(image);
        }
        let (source_width, source_height) = probe_source(kind, bytes)?;
        let variant = match target {
            None => ORIGINAL_TARGET,
            Some(target) => resolve_target(kind, target, (source_width, source_height)),
        };
        if let Some(image) = self.dedupe_hit(bytes, variant, kind) {
            return Ok(image);
        }
        let reservation = self.admit_decode(source_width, source_height, bytes.len())?;
        let decoded = match decode_source(kind, bytes, target.map(|_| variant)) {
            Ok(decoded) => decoded,
            Err(cause) => {
                self.release_reservation(reservation);
                return Err(cause);
            }
        };
        self.release_reservation(reservation);
        self.insert_decoded(
            Bytes::copy_from_slice(bytes),
            decoded,
            variant,
            kind,
            logical,
            (source_width, source_height),
        )
    }

    pub fn insert_owned(
        &mut self,
        owner: WindowId,
        bytes: &[u8],
    ) -> Result<ResourceImage, EngineError> {
        let image = self.insert(bytes)?;
        self.owners.entry(owner).or_default().push(image.id);
        Ok(image)
    }

    pub fn insert_owned_at_size(
        &mut self,
        owner: WindowId,
        bytes: &[u8],
        target: LogicalSize,
    ) -> Result<ResourceImage, EngineError> {
        let image = self.insert_at_size(bytes, target)?;
        self.owners.entry(owner).or_default().push(image.id);
        Ok(image)
    }

    pub fn insert_owned_svg_for_display(
        &mut self,
        owner: WindowId,
        bytes: &[u8],
        logical: LogicalSize,
        scale: f32,
    ) -> Result<ResourceImage, EngineError> {
        let image = self.insert_svg_for_display(bytes, logical, scale)?;
        self.owners.entry(owner).or_default().push(image.id);
        Ok(image)
    }

    pub(crate) fn record_owner(&mut self, owner: WindowId, id: ResourceId) {
        self.owners.entry(owner).or_default().push(id);
    }

    pub(crate) fn reselect(&mut self, id: ResourceId, scale: f32) -> Option<ResourceImage> {
        let (key, target, intrinsic, logical, kind) = {
            let entry = self.entries.get(id)?;
            (
                entry.key,
                entry.target,
                entry.intrinsic,
                entry.logical?,
                entry.kind,
            )
        };
        let variant = resolve_target(kind, physical_target(logical, scale), intrinsic);
        if variant == target {
            return self.entries.get(id).map(|entry| entry.image);
        }
        let bytes = self.dedupe.get(&key)?.bytes.clone();
        if let Some(image) = self.dedupe_hit(&bytes, variant, kind) {
            return Some(image);
        }
        let decoded = decode_source(kind, &bytes, Some(variant)).ok()?;
        self.insert_decoded(bytes, decoded, variant, kind, Some(logical), intrinsic)
            .ok()
    }

    pub fn insert_async(&mut self, bytes: Bytes) -> PendingResource {
        self.insert_async_inner(bytes, None, SourceKind::Bitmap, None)
    }

    pub fn insert_async_at_size(&mut self, bytes: Bytes, target: LogicalSize) -> PendingResource {
        self.insert_async_inner(
            bytes,
            Some(logical_target(target)),
            SourceKind::Bitmap,
            None,
        )
    }

    pub fn insert_svg_async_at_size(
        &mut self,
        bytes: Bytes,
        target: LogicalSize,
    ) -> PendingResource {
        self.insert_async_inner(bytes, Some(logical_target(target)), SourceKind::Svg, None)
    }

    fn insert_async_inner(
        &mut self,
        bytes: Bytes,
        target: Option<PhysicalSize>,
        kind: SourceKind,
        logical: Option<LogicalSize>,
    ) -> PendingResource {
        if let Err(cause) = image::check_source_bytes(bytes.len()) {
            return self.push_failed(cause);
        }
        if target.is_none()
            && let Some(image) = self.dedupe_hit(&bytes, ORIGINAL_TARGET, kind)
        {
            return self.push_ready(image);
        }
        let (source_width, source_height) = match probe_source(kind, &bytes) {
            Ok(dimensions) => dimensions,
            Err(cause) => return self.push_failed(cause),
        };
        let variant = match target {
            None => ORIGINAL_TARGET,
            Some(target) => resolve_target(kind, target, (source_width, source_height)),
        };
        if let Some(image) = self.dedupe_hit(&bytes, variant, kind) {
            return self.push_ready(image);
        }
        let reservation = match self.admit_decode(source_width, source_height, bytes.len()) {
            Ok(reservation) => reservation,
            Err(cause) => return self.push_failed(cause),
        };
        let decode_target = target.map(|_| variant);
        let intrinsic = (source_width, source_height);
        let Some(runtime) = self.runtime.clone() else {
            self.release_reservation(reservation);
            let state = match decode_source(kind, &bytes, decode_target) {
                Ok(decoded) => {
                    self.finish_decoded(bytes, decoded, variant, intrinsic, kind, logical)
                }
                Err(cause) => DecodeState::Failed(cause),
            };
            return self.push_pending(PendingJob {
                result: None,
                state,
                source: Bytes::new(),
                reservation: 0,
                target: variant,
                intrinsic,
                kind,
                logical,
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
                let result = decode_source(kind, &source, decode_target);
                guard.deliver(result);
            });
        });
        self.push_pending(PendingJob {
            result: Some(receiver),
            state: DecodeState::Pending,
            source: bytes,
            reservation,
            target: variant,
            intrinsic,
            kind,
            logical,
        })
    }

    pub fn poll(&mut self) -> Vec<(PendingResource, DecodeState)> {
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
                Ok(decoded) => self.finish_decoded(
                    job.source,
                    decoded,
                    job.target,
                    job.intrinsic,
                    job.kind,
                    job.logical,
                ),
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
        self.entries.get(id).map(|entry| entry.image)
    }

    fn push_pending(&mut self, job: PendingJob) -> PendingResource {
        let id = self.pending.insert_with(|_| job);
        self.pending_ids.push(id);
        id
    }

    fn push_failed(&mut self, cause: DecodeCause) -> PendingResource {
        self.push_pending(PendingJob {
            result: None,
            state: DecodeState::Failed(cause),
            source: Bytes::new(),
            reservation: 0,
            target: ORIGINAL_TARGET,
            intrinsic: ORIGINAL_TARGET,
            kind: SourceKind::Bitmap,
            logical: None,
        })
    }

    fn push_ready(&mut self, image: ResourceImage) -> PendingResource {
        self.push_pending(PendingJob {
            result: None,
            state: DecodeState::Ready(image),
            source: Bytes::new(),
            reservation: 0,
            target: ORIGINAL_TARGET,
            intrinsic: ORIGINAL_TARGET,
            kind: SourceKind::Bitmap,
            logical: None,
        })
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

    fn finish_decoded(
        &mut self,
        source: Bytes,
        decoded: DecodedImage,
        target: (u32, u32),
        intrinsic: (u32, u32),
        kind: SourceKind,
        logical: Option<LogicalSize>,
    ) -> DecodeState {
        if let Some(image) = self.dedupe_hit(&source, target, kind) {
            return DecodeState::Ready(image);
        }
        match self.insert_decoded(source, decoded, target, kind, logical, intrinsic) {
            Ok(image) => DecodeState::Ready(image),
            Err(cause) => DecodeState::Failed(cause),
        }
    }

    fn dedupe_hit(
        &mut self,
        bytes: &[u8],
        target: (u32, u32),
        kind: SourceKind,
    ) -> Option<ResourceImage> {
        let key = ContentKey::new(bytes, kind);
        let existing = self.dedupe.get(&key)?;
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
            self.remove_variant(key, target, id);
            return None;
        };
        entry.refcount = entry.refcount.saturating_add(1);
        entry.last_used = epoch;
        Some(entry.image)
    }

    fn insert_decoded(
        &mut self,
        source: Bytes,
        decoded: DecodedImage,
        target: (u32, u32),
        kind: SourceKind,
        logical: Option<LogicalSize>,
        intrinsic: (u32, u32),
    ) -> Result<ResourceImage, DecodeCause> {
        let color = decoded.color;
        let key = ContentKey::new(&source, kind);
        let image = self.place(decoded, key, target, kind, logical, intrinsic)?;
        match color {
            SourceColor::Srgb => {}
            SourceColor::Converted => {
                self.color_converted = self.color_converted.saturating_add(1);
            }
            SourceColor::AssumedSrgb => {
                self.color_fallback = self.color_fallback.saturating_add(1);
            }
        }
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
            self.source_bytes = self
                .source_bytes
                .saturating_sub(replaced.bytes.len() as u64);
        }
        Ok(image)
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

    fn place(
        &mut self,
        decoded: DecodedImage,
        key: ContentKey,
        target: (u32, u32),
        kind: SourceKind,
        logical: Option<LogicalSize>,
        intrinsic: (u32, u32),
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
            intrinsic,
            kind,
            logical,
        });
        self.used_pixels = self
            .used_pixels
            .saturating_add(u64::from(width) * u64::from(height));
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

    pub fn release_window(&mut self, owner: WindowId) {
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
        math::{DeviceRect, LogicalSize, PhysicalSize, dp, physical_px, px},
        paint::atlas::{AtlasKind, MAX_IMAGE_PAGES},
        resources::image::{MAX_SOURCE_BYTES, MAX_SOURCE_DIM, MAX_SOURCE_PIXELS},
        window::WindowId,
    };

    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        encoded(width, height, rgba, image::ImageFormat::Png)
    }

    fn encoded(width: u32, height: u32, rgba: [u8; 4], format: image::ImageFormat) -> Vec<u8> {
        let pixels: Vec<u8> = std::iter::repeat_n(rgba, (width * height) as usize)
            .flatten()
            .collect();
        rgba_png(width, height, &pixels, format)
    }

    fn rgba_png(width: u32, height: u32, pixels: &[u8], format: image::ImageFormat) -> Vec<u8> {
        let image = image::RgbaImage::from_raw(width, height, pixels.to_vec()).expect("raw");
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, format)
            .expect("encode");
        out.into_inner()
    }

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

    #[test]
    fn page_capacity_is_independent_of_insertion_order() {
        let mut cache = ResourceCache::new_unmanaged();
        let tiny = inserted(&mut cache, [1, 2, 3, 255]);
        assert_eq!((tiny.width, tiny.height), (4, 4));
        let big = cache.insert(&png(600, 600, [9, 9, 9, 255])).expect(
            "600x600 must fit after a tiny image (the page is not locked by the first insert)",
        );
        assert_eq!((big.width, big.height), (600, 600));
        assert_eq!(tiny.tile.atlas, big.tile.atlas);
        assert_eq!(cache.len(), 2);
    }

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
        assert_eq!(stats.used_bytes, 4 * 8 * 8 * 4 + source);
        let expected = (images.len() as f32 * 36.0) / (f32::from(MAX_IMAGE_PAGES) * 64.0);
        assert!(
            (stats.page_occupancy - expected).abs() < 1e-6,
            "占用率按全部页数计：{} vs {expected}",
            stats.page_occupancy
        );
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
        assert!(!rects.is_full(), "X4：9 块图片不得折叠成整页哨兵");
        let page_bytes = 2048u64 * 2048 * 4;
        let uploaded = rects.byte_count(2048, 4) as u64;
        assert!(
            uploaded < page_bytes,
            "X4：上传字节 {uploaded} 必须小于整页 {page_bytes}"
        );
        assert_eq!(rects.chunks(2048, 4).count(), 1, "折叠结果是单个 bbox 矩形");
    }

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

    #[test]
    fn page_buffers_and_dirty_rects_are_per_page() {
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let first = cache.insert(&png(6, 6, [11, 22, 33, 255])).expect("first");
        let second = cache.insert(&png(6, 6, [44, 55, 66, 255])).expect("second");
        assert_eq!((first.tile.page, second.tile.page), (0, 1));
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
        let _ = cache.take_dirty_rects();
        let before: Vec<u8> = cache.page_pixels().expect("page").1.to_vec();
        assert!(before.iter().any(|byte| *byte != 0), "插入必须写进页");

        assert!(cache.unload(image.id));
        let after = cache.page_pixels().expect("page").1;
        assert!(after.iter().all(|byte| *byte == 0), "卸载后区域必须清零");
        let rects = cache.take_dirty_rects();
        assert!(!rects.is_empty(), "清零区必须上报脏矩形");
        assert_eq!(rects.rects(), &[region]);
    }

    #[test]
    fn first_dirty_rects_cover_the_whole_page() {
        let mut cache = cache();
        let rects = cache.take_dirty_rects();
        assert_eq!(rects.rects().len(), 1);
        assert_eq!(
            rects.rects()[0],
            DeviceRect::new(dp(0), dp(0), dp(16), dp(16)),
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

    fn image_region(cache: &ResourceCache, image: ResourceImage) -> DeviceRect {
        cache.entries.get(image.id).expect("entry").padded
    }

    #[test]
    fn page_pixels_hold_premultiplied_bgra() {
        let mut cache = cache();
        let image = inserted(&mut cache, [128, 128, 128, 255]);
        let (dimensions, pixels) = cache.page_pixels().expect("page");
        assert_eq!(dimensions, (16, 16));

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

        cache.release_window(right);
        assert_eq!(cache.len(), 0);
    }

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

    #[test]
    fn new_page_allocation_is_admitted_by_the_byte_budget() {
        let page_bytes = 16u64 * 16 * 4;
        let first_bytes = png(14, 14, [10, 20, 30, 255]);
        let second_bytes = png(14, 14, [40, 50, 60, 255]);
        let third_bytes = png(14, 14, [70, 80, 90, 255]);
        let source = |bytes: &[u8]| bytes.len() as u64;
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

    fn content_pixel(cache: &ResourceCache, image: ResourceImage) -> [u8; 4] {
        content_pixel_at(cache, image, 0, 0)
    }

    fn content_pixel_at(cache: &ResourceCache, image: ResourceImage, x: u32, y: u32) -> [u8; 4] {
        assert!(x < image.width && y < image.height, "越界采样 {x},{y}");
        let ((page_width, page_height), pixels) = cache.page_pixels().expect("page");
        let origin_x = (image.tile.u0 * page_width as f32).round() as usize;
        let origin_y = (image.tile.v0 * page_height as f32).round() as usize;
        let offset = ((origin_y + y as usize) * page_width as usize + origin_x + x as usize) * 4;
        pixels[offset..offset + 4].try_into().expect("4 bytes")
    }

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

    #[test]
    fn icc_conversion_is_counted_once_per_decode() {
        use super::image::test_png;

        let mut cache = ResourceCache::with_page_for_test((64, 64));
        let p3 = moxcms::ColorProfile::new_display_p3()
            .encode()
            .expect("encode display p3");
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

    #[test]
    fn insert_at_size_downscales_and_keeps_aspect() {
        let mut cache = ResourceCache::with_page_for_test((128, 128));
        let bytes = quadrant_png(200, 100);
        let image = cache
            .insert_at_size(&bytes, LogicalSize::new(px(40.0), px(40.0)))
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

    #[test]
    fn insert_at_size_never_upscales() {
        let mut cache = ResourceCache::with_page_for_test((128, 128));
        let bytes = png(40, 20, [90, 120, 150, 255]);
        let image = cache
            .insert_at_size(&bytes, LogicalSize::new(px(200.0), px(200.0)))
            .expect("目标大于源");

        assert_eq!(
            (image.width, image.height),
            (40, 20),
            "不得把 40×20 放大成 200×200（应用负责提供 2× 资产）"
        );
        let original = cache.insert(&bytes).expect("原始尺寸变体");
        assert_eq!((original.width, original.height), (40, 20));
        assert_eq!(
            tile_pixels(&cache, image),
            tile_pixels(&cache, original),
            "不放大时像素必须与原始尺寸路径一致"
        );
        assert_eq!(cache.len(), 2, "按目标尺寸与原始尺寸是两个变体");
    }

    #[test]
    fn variants_of_the_same_content_coexist_and_refcount_independently() {
        let mut cache = ResourceCache::with_page_for_test((128, 128));
        let bytes = png(200, 100, [30, 60, 90, 255]);
        let large = cache
            .insert_at_size(&bytes, LogicalSize::new(px(80.0), px(80.0)))
            .expect("大缩略图");
        let small = cache
            .insert_at_size(&bytes, LogicalSize::new(px(20.0), px(20.0)))
            .expect("小缩略图");

        assert_ne!(large.id, small.id, "不同 target 是不同的变体");
        assert_eq!((large.width, large.height), (80, 40));
        assert_eq!((small.width, small.height), (20, 10));
        assert_eq!(cache.len(), 2, "两个变体各占一条 Entry");
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().used_pixels, 80 * 40 + 20 * 10);
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

        assert!(cache.unload(large.id));
        assert_eq!(cache.len(), 1, "另一个变体仍存活");
        assert_eq!(cache.stats().used_pixels, 20 * 10);
        assert_eq!(cache.source_bytes, bytes.len() as u64, "还有变体存活");
        let small_again = cache
            .insert_at_size(&bytes, LogicalSize::new(px(20.0), px(20.0)))
            .expect("小变体仍存活，命中去重");
        assert_eq!(small_again, small, "同 (bytes, target) 去重命中");
        assert_eq!(cache.len(), 1);
        assert_eq!(
            content_pixel(&cache, small_again),
            content_pixel(&cache, small),
            "去重命中后 tile 内容仍是原来的变体"
        );
        assert!(cache.unload(small_again.id));
        assert_eq!(cache.len(), 1);
        assert!(cache.unload(small.id));
        assert_eq!(cache.len(), 0, "最后一个变体回收");
        assert!(cache.dedupe.is_empty(), "全部变体归零才删去重条目");
        assert_eq!(cache.source_bytes, 0, "全部变体归零才释放源字节");
        assert_eq!(cache.stats().used_bytes, 128 * 128 * 4, "只剩图片页常驻");
    }

    #[test]
    fn unload_releases_the_only_variant_and_source_bytes() {
        let mut cache = ResourceCache::with_page_for_test((64, 64));
        let bytes = png(200, 100, [10, 20, 30, 255]);
        let image = cache
            .insert_at_size(&bytes, LogicalSize::new(px(40.0), px(40.0)))
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
        assert!(!cache.unload(image.id));
    }

    #[test]
    fn insert_for_display_rounds_to_physical_pixels_and_does_not_upscale() {
        let mut cache = ResourceCache::with_page_for_test((256, 256));
        let wide = png(200, 100, [1, 2, 3, 255]);
        let hd = cache
            .insert_for_display(&wide, LogicalSize::new(px(100.0), px(50.0)), 1.5)
            .expect("1.5x");
        assert_eq!((hd.width, hd.height), (150, 75));

        let square = png(100, 100, [7, 8, 9, 255]);
        let rounded = cache
            .insert_for_display(&square, LogicalSize::new(px(30.2), px(30.2)), 2.0)
            .expect("ceil 物理像素");
        assert_eq!((rounded.width, rounded.height), (61, 61));

        let small = png(40, 20, [4, 5, 6, 255]);
        let at_2x = cache
            .insert_for_display(&small, LogicalSize::new(px(40.0), px(20.0)), 2.0)
            .expect("2x");
        assert_eq!((at_2x.width, at_2x.height), (40, 20));

        for scale in [0.0f32, -2.0, f32::NAN, f32::INFINITY] {
            let defensive = cache
                .insert_for_display(&small, LogicalSize::new(px(40.0), px(20.0)), scale)
                .expect("防御 scale");
            assert_eq!(
                (defensive.width, defensive.height),
                (40, 20),
                "scale {scale} 必须按 1.0 处理"
            );
        }
    }

    #[test]
    fn async_insert_at_size_delivers_the_requested_variant() {
        let runtime = runtime();
        let mut cache = ResourceCache::for_test(
            (128, 128),
            DEFAULT_BYTE_BUDGET,
            Some(runtime.handle().clone()),
        );
        let bytes = png(200, 100, [12, 34, 56, 255]);
        let pending =
            cache.insert_async_at_size(Bytes::from(bytes), LogicalSize::new(px(40.0), px(40.0)));
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

        let again = cache.insert_async_at_size(
            Bytes::from(png(200, 100, [12, 34, 56, 255])),
            LogicalSize::new(px(40.0), px(40.0)),
        );
        assert_eq!(
            cache.state(again),
            DecodeState::Ready(image),
            "异步去重命中立即 Ready、不重新解码"
        );
        assert_eq!(cache.poll().len(), 1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn async_decode_admission_rejects_before_spawning() {
        let runtime = runtime();
        let bytes = png(200, 200, [1, 2, 3, 255]);
        let page_bytes = 16 * 16 * 4;
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

        let delivered = cache.poll();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, pending);
        assert_eq!(delivered[0].1, state);
        assert!(cache.poll().is_empty(), "交付后不得重复");
    }

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

    #[test]
    fn async_decode_reservations_are_returned_after_completion() {
        let runtime = runtime();
        let first_bytes = png(4, 4, [1, 2, 3, 255]);
        let second_bytes = png(4, 4, [4, 5, 6, 255]);
        let page_bytes = 16 * 16 * 4;
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

        let second = cache.insert_async(Bytes::from(second_bytes));
        assert_eq!(cache.state(second), DecodeState::Pending);
        let state = poll_until(&mut cache, second);
        assert!(matches!(state, DecodeState::Ready(_)), "{state:?}");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.reserved_decode_bytes, 0);
    }

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

    fn svg(width: f32, height: f32) -> Vec<u8> {
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" \
             viewBox=\"0 0 {width} {height}\"><path d=\"M2 2H{width}V{height}H2Z\" \
             fill=\"#FFFFFF\"/></svg>"
        )
        .into_bytes()
    }

    fn svg_cache() -> ResourceCache {
        ResourceCache::with_page_for_test((128, 128))
    }

    #[test]
    fn svg_resources_rasterize_at_the_requested_physical_size() {
        let mut cache = svg_cache();
        let bytes = svg(16.0, 16.0);
        let small = cache
            .insert_svg_at_size(&bytes, LogicalSize::new(px(16.0), px(16.0)))
            .expect("16px 栅格");
        assert_eq!((small.width, small.height), (16, 16));
        let large = cache
            .insert_svg_at_size(&bytes, LogicalSize::new(px(32.0), px(32.0)))
            .expect("32px 栅格");
        assert_eq!((large.width, large.height), (32, 32));
        assert_ne!(small.id, large.id, "不同物理尺寸必须是不同变体");
        let again = cache
            .insert_svg_at_size(&bytes, LogicalSize::new(px(16.0), px(16.0)))
            .expect("去重命中");
        assert_eq!(again.id, small.id, "同内容同尺寸必须命中已有变体");
        assert_eq!(cache.stats().entries, 2, "只应有两份变体");
    }

    #[test]
    fn svg_resources_upscale_above_the_intrinsic_size() {
        let mut cache = svg_cache();
        let image = cache
            .insert_svg_at_size(&svg(16.0, 16.0), LogicalSize::new(px(48.0), px(48.0)))
            .expect("48px 栅格");
        assert_eq!(
            (image.width, image.height),
            (48, 48),
            "SVG 必须能向上栅格（位图不会放大）"
        );
    }

    #[test]
    fn svg_targets_keep_the_intrinsic_aspect_ratio() {
        let mut cache = svg_cache();
        let image = cache
            .insert_svg_at_size(&svg(20.0, 10.0), LogicalSize::new(px(40.0), px(40.0)))
            .expect("fit 栅格");
        assert_eq!(
            (image.width, image.height),
            (40, 20),
            "非方形 SVG 必须等比装进目标盒"
        );
    }

    #[test]
    fn svg_reselect_follows_the_scale_and_reuses_variants() {
        let mut cache = svg_cache();
        let bytes = svg(16.0, 16.0);
        let one_x = cache
            .insert_svg_for_display(&bytes, LogicalSize::new(px(16.0), px(16.0)), 1.0)
            .expect("1× 变体");
        assert_eq!((one_x.width, one_x.height), (16, 16));
        let two_x = cache.reselect(one_x.id, 2.0).expect("2× 变体");
        assert_eq!((two_x.width, two_x.height), (32, 32));
        assert_ne!(two_x.id, one_x.id);
        let back = cache.reselect(two_x.id, 1.0).expect("切回 1×");
        assert_eq!(back.id, one_x.id, "切回旧 scale 必须复用已有变体");
    }

    #[test]
    fn bitmap_display_variants_also_reselect_with_the_scale() {
        let mut cache = svg_cache();
        let bytes = png(80, 40, [200, 10, 10, 255]);
        let one_x = cache
            .insert_for_display(&bytes, LogicalSize::new(px(40.0), px(20.0)), 1.0)
            .expect("1× 变体");
        assert_eq!((one_x.width, one_x.height), (40, 20));
        let two_x = cache.reselect(one_x.id, 2.0).expect("2× 变体");
        assert_eq!((two_x.width, two_x.height), (80, 40));
    }

    #[test]
    fn svg_decode_failures_are_typed() {
        let mut cache = svg_cache();
        let error = cache.insert_svg(b"<svg").expect_err("损坏的 SVG 必须失败");
        assert!(
            matches!(
                error,
                EngineError::ResourceDecode {
                    cause: DecodeCause::Corrupt { .. }
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn svg_probe_uses_the_declared_size() {
        assert_eq!(
            super::svg::probe_dimensions(&svg(16.0, 16.0)).expect("probe"),
            (16, 16)
        );
        assert_eq!(
            super::svg::probe_dimensions(&svg(20.0, 10.0)).expect("probe"),
            (20, 10)
        );
        assert_eq!(
            super::svg::resolve_target(
                PhysicalSize::new(physical_px(40.0), physical_px(40.0)),
                (20, 10),
            ),
            (40, 20),
            "目标解析必须等比"
        );
        assert_eq!(
            super::svg::resolve_target(
                PhysicalSize::new(physical_px(20.0), physical_px(20.0)),
                (16, 16),
            ),
            (20, 20),
            "SVG 允许向上栅格"
        );
    }

    #[test]
    fn svg_decode_produces_premultiplied_bgra_with_analytic_edges() {
        let decoded = super::svg::decode(&svg(16.0, 16.0), Some((16, 16))).expect("decode");
        assert_eq!((decoded.width, decoded.height), (16, 16));
        assert_eq!(
            &decoded.bgra[(8 * 16 + 8) * 4..(8 * 16 + 8) * 4 + 4],
            &[255, 255, 255, 255],
            "白色填充的中心像素必须是 BGRA 全白不透明"
        );
        assert_eq!(
            &decoded.bgra[0..4],
            &[0, 0, 0, 0],
            "premultiplied 的空白像素必须是全 0（不需要白 matte）"
        );

        let triangle = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\" \
                         viewBox=\"0 0 16 16\"><path d=\"M8 0L16 16L0 16Z\" fill=\"#FFFFFF\"/>\
                         </svg>";
        let decoded = super::svg::decode(triangle, Some((16, 16))).expect("decode triangle");
        assert!(
            (0..decoded.width as usize * decoded.height as usize).any(|index| {
                let alpha = decoded.bgra[index * 4 + 3];
                alpha > 0 && alpha < 255
            }),
            "斜边必须带解析抗锯齿（部分 alpha 像素）"
        );
    }
}

use smallvec::SmallVec;

use crate::{
    error::EngineError,
    math::{
        DevicePixelRatio, DeviceRect, LogicalPoint, LogicalPx, LogicalRect, PhysicalPx, Transform,
        dp, px,
    },
    paint::{
        atlas::MAX_IMAGE_PAGES,
        color::{ColorResolver, LinearPremulRgba},
        primitive::{ClipOp, GradientKind, Primitive, QuadPaint},
        scene::MAX_CLIP_DEPTH,
    },
};

pub(crate) trait LaneSource {
    fn lane_state(&self, id: crate::paint::scene::LaneId) -> Option<(u32, &[Primitive])>;

    fn order(&self) -> &[crate::paint::scene::LaneId];

    fn lane_scroll(&self, _id: crate::paint::scene::LaneId) -> u32 {
        0
    }

    fn lane_geometry_chunk(
        &self,
        _id: crate::paint::scene::LaneId,
    ) -> Option<(crate::paint::geometry::GeometryId, u32)> {
        None
    }

    fn changed_lanes(&self) -> Option<&[crate::paint::scene::LaneId]> {
        None
    }
}

pub(crate) trait GeometryLookup {
    fn snapshot_arc(
        &self,
        id: crate::paint::geometry::GeometryId,
    ) -> Option<std::sync::Arc<crate::paint::geometry::GeometrySnapshot>>;
}

impl GeometryLookup for crate::paint::geometry_store::GeometryStore {
    fn snapshot_arc(
        &self,
        id: crate::paint::geometry::GeometryId,
    ) -> Option<std::sync::Arc<crate::paint::geometry::GeometrySnapshot>> {
        crate::paint::geometry_store::GeometryStore::snapshot_arc(self, id)
    }
}

#[repr(C, align(16))]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuPrimitive {
    pub bounds: [f32; 4],
    pub corner_radius: [f32; 4],
    pub color0: [f32; 4],
    pub color1: [f32; 4],
    pub params0: [f32; 4],
    pub params1: [f32; 4],
    pub uv: [f32; 4],
    pub clip_rect: [f32; 4],
}

const _: () = {
    use core::mem::{align_of, offset_of, size_of};

    assert!(size_of::<GpuPrimitive>() == 128);
    assert!(align_of::<GpuPrimitive>() == 16);
    assert!(offset_of!(GpuPrimitive, bounds) == 0);
    assert!(offset_of!(GpuPrimitive, corner_radius) == 16);
    assert!(offset_of!(GpuPrimitive, color0) == 32);
    assert!(offset_of!(GpuPrimitive, color1) == 48);
    assert!(offset_of!(GpuPrimitive, params0) == 64);
    assert!(offset_of!(GpuPrimitive, params1) == 80);
    assert!(offset_of!(GpuPrimitive, uv) == 96);
    assert!(offset_of!(GpuPrimitive, clip_rect) == 112);
};

pub(crate) const KIND_QUAD: f32 = 0.0;
pub(crate) const KIND_GLYPH: f32 = 1.0;
pub(crate) const KIND_IMAGE: f32 = 2.0;
pub(crate) const KIND_SHADOW: f32 = 3.0;
pub(crate) const KIND_COLOR_GLYPH: f32 = 8.0;

pub(crate) const GRADIENT_NONE: f32 = 0.0;
pub(crate) const GRADIENT_HORIZONTAL: f32 = 1.0;
pub(crate) const GRADIENT_VERTICAL: f32 = 2.0;
pub(crate) const GRADIENT_RADIAL: f32 = 3.0;

pub(crate) const SHADOW_SIGMA_SCALE: f32 = 0.5;

pub(crate) const SHADOW_SIGMA_MIN: f32 = 0.35;

pub(crate) const SHADOW_PAD_SIGMAS: f32 = 3.0;

pub(crate) const CLIP_KIND_RECT: f32 = 1.0;

pub(crate) const CLIP_KIND_ROUNDED: f32 = 2.0;

#[cfg(any(debug_assertions, test))]
pub(crate) const MAX_CLIP_ITEMS: f32 = 9.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClipChainId(u32);

impl ClipChainId {
    #[cfg(any(debug_assertions, test))]
    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipKind {
    Rect,
    Rounded,
}

impl ClipKind {
    fn gpu(self) -> u32 {
        match self {
            Self::Rect => CLIP_KIND_RECT as u32,
            Self::Rounded => CLIP_KIND_ROUNDED as u32,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClipChain {
    parent: Option<ClipChainId>,
    kind: ClipKind,
    rect: [f32; 4],
    corner_radius: [f32; 4],
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuClipEntry {
    pub rect: [f32; 4],
    pub corner_radius: [f32; 4],
    pub meta: [u32; 4],
}

const _: () = {
    use core::mem::{align_of, size_of};
    assert!(size_of::<GpuClipEntry>() == 48);
    assert!(align_of::<GpuClipEntry>() == 16);
};

impl ClipChain {
    pub(crate) fn gpu_entry(self) -> GpuClipEntry {
        GpuClipEntry {
            rect: self.rect,
            corner_radius: self.corner_radius,
            meta: [
                self.parent.map_or(0, |parent| parent.0),
                self.kind.gpu(),
                0,
                0,
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameOp {
    Instances {
        start: u32,
        len: u32,
        bounds: DeviceRect,
        scroll: u32,
    },
    Geometry {
        index: u32,
        clip_index: u32,
        clip_rect: [u32; 4],
        bounds: DeviceRect,
    },
}

impl FrameOp {
    pub(crate) fn bounds(self) -> DeviceRect {
        match self {
            Self::Instances { bounds, .. } | Self::Geometry { bounds, .. } => bounds,
        }
    }

    pub(crate) fn scroll(self) -> u32 {
        match self {
            Self::Instances { scroll, .. } => scroll,
            Self::Geometry { .. } => 0,
        }
    }
}

pub(crate) fn scroll_offset(scroll: &[[f32; 2]], op: FrameOp) -> [f32; 2] {
    scroll
        .get(op.scroll() as usize)
        .copied()
        .unwrap_or([0.0, 0.0])
}

pub(crate) fn scrolled_scissor(
    scroll_clip: &[[i32; 4]],
    slot: u32,
    scissor: [i32; 4],
) -> Option<[i32; 4]> {
    if slot == 0 {
        return None;
    }
    let clip = scroll_clip.get(slot as usize)?;
    Some([
        scissor[0].max(clip[0]),
        scissor[1].max(clip[1]),
        scissor[2].min(clip[2]),
        scissor[3].min(clip[3]),
    ])
}

fn run_bounds(instances: &[GpuPrimitive]) -> DeviceRect {
    instances
        .iter()
        .filter_map(instance_bounds)
        .reduce(DeviceRect::union)
        .unwrap_or_default()
}

fn instance_bounds(primitive: &GpuPrimitive) -> Option<DeviceRect> {
    let (Some(extent), Some(clip)) = (instance_extent(primitive), clip_extent(primitive)) else {
        return None;
    };
    let x0 = extent[0].max(clip[0]);
    let y0 = extent[1].max(clip[1]);
    let x1 = extent[2].min(clip[2]);
    let y1 = extent[3].min(clip[3]);
    if x0 > x1 || y0 > y1 {
        return None;
    }
    Some(DeviceRect::new(dp(x0), dp(y0), dp(x1), dp(y1)))
}

fn bounds_of_aabb(aabb: [f32; 4]) -> DeviceRect {
    if aabb.iter().any(|value| value.is_nan()) {
        return DeviceRect::default();
    }
    let [x0, y0, x1, y1] = aabb;
    if x1 < x0 || y1 < y0 {
        return DeviceRect::default();
    }
    DeviceRect {
        x0: dp((x0.floor() as i32).saturating_sub(1)),
        y0: dp((y0.floor() as i32).saturating_sub(1)),
        x1: dp((x1.ceil() as i32).saturating_add(1)),
        y1: dp((y1.ceil() as i32).saturating_add(1)),
    }
}

fn instance_extent(primitive: &GpuPrimitive) -> Option<[i32; 4]> {
    let [x, y, w, h] = primitive.bounds;
    let pad = primitive.params0[3];
    extent_of(x, y, w, h, pad)
}

fn is_unclipped(clip_rect: [f32; 4]) -> bool {
    clip_rect[2] >= 1.0e30 && clip_rect[3] >= 1.0e30
}

fn clip_extent(primitive: &GpuPrimitive) -> Option<[i32; 4]> {
    let clip_rect = primitive.clip_rect;
    if clip_rect.iter().any(|value| value.is_nan()) {
        return None;
    }
    if is_unclipped(clip_rect) {
        return Some([i32::MIN, i32::MIN, i32::MAX, i32::MAX]);
    }
    let [x, y, w, h] = clip_rect;
    if w < 0.0 || h < 0.0 {
        return None;
    }
    Some([
        x.floor() as i32,
        y.floor() as i32,
        ((x + w).ceil() as i32).saturating_add(1),
        ((y + h).ceil() as i32).saturating_add(1),
    ])
}

fn extent_of(x: f32, y: f32, w: f32, h: f32, pad: f32) -> Option<[i32; 4]> {
    if ![x, y, w, h, pad].iter().all(|value| value.is_finite()) {
        return None;
    }
    let pad = pad.max(0.0).ceil() as i32 + 1;
    Some([
        (x.floor() as i32).saturating_sub(pad),
        (y.floor() as i32).saturating_sub(pad),
        ((x + w).ceil() as i32).saturating_add(pad),
        ((y + h).ceil() as i32).saturating_add(pad),
    ])
}

const MAX_LANES_PER_OP: u32 = 64;

const CLIP_TABLE_LIMIT: usize = 2048;

const CHANGE_MERGE_GAP: u32 = 8;

struct LaneSegment {
    generation: u32,
    version: u32,
    scroll: u32,
    start: u32,
    end: u32,
    parts: SmallVec<[LanePart; 2]>,
}

impl LaneSegment {
    fn range(&self) -> std::ops::Range<u32> {
        self.start..self.end
    }

    fn len(&self) -> u32 {
        self.end - self.start
    }
}

enum LanePart {
    Instances { len: u32, bounds: DeviceRect },
    Geometry(Box<LaneGeometry>),
}

struct LaneGeometry {
    draw: GeometryDrawGpu,
    clip_index: u32,
    clip_rect: [u32; 4],
    bounds: DeviceRect,
}

#[derive(Default)]
struct ChangeSet {
    ranges: Vec<std::ops::Range<u32>>,
    sorted: bool,
}

impl ChangeSet {
    fn clear(&mut self) {
        self.ranges.clear();
        self.sorted = true;
    }

    fn push(&mut self, range: std::ops::Range<u32>) {
        if range.is_empty() {
            return;
        }
        if self
            .ranges
            .last()
            .is_some_and(|last| last.start > range.start)
        {
            self.sorted = false;
        }
        self.ranges.push(range);
    }

    fn merged(&mut self) -> &[std::ops::Range<u32>] {
        if !self.sorted {
            self.ranges.sort_unstable_by_key(|range| range.start);
            self.sorted = true;
        }
        let mut out = 0usize;
        for read in 0..self.ranges.len() {
            let range = self.ranges[read].clone();
            if out > 0 {
                let last = &mut self.ranges[out - 1];
                if range.start <= last.end.saturating_add(CHANGE_MERGE_GAP) {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            self.ranges[out] = range;
            out += 1;
        }
        self.ranges.truncate(out);
        &self.ranges
    }
}

pub(crate) struct InstanceBuffer {
    instances: Vec<GpuPrimitive>,
    free: Vec<std::ops::Range<u32>>,
    used: u32,
    clips: Vec<ClipChain>,
    geometry: Vec<GeometryDrawGpu>,
    ops: Vec<FrameOp>,
    segments: Vec<Option<Box<LaneSegment>>>,
    changed: ChangeSet,
}

#[derive(Clone, Debug)]
pub(crate) struct GeometryDrawGpu {
    pub id: crate::paint::geometry::GeometryId,
    pub range: std::ops::Range<u32>,
    pub transform: Transform,
    pub color: [f32; 4],
    pub snapshot: std::sync::Arc<crate::paint::geometry::GeometrySnapshot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DrawIndexedArgs {
    pub first_index: u32,
    pub count: u32,
    pub base_vertex: i32,
}

pub(crate) fn draw_indexed_params(
    first_index: u32,
    count: u32,
    uploaded_index_count: u32,
) -> Result<DrawIndexedArgs, EngineError> {
    if count == 0 {
        return Err(EngineError::GeometryRegister {
            detail: smol_str::format_smolstr!(
                "empty DrawIndexed range (first_index {first_index})"
            ),
        });
    }
    let end = first_index
        .checked_add(count)
        .ok_or_else(|| EngineError::GeometryRegister {
            detail: smol_str::format_smolstr!(
                "DrawIndexed range {first_index}+{count} overflows the u32 index space"
            ),
        })?;
    if end > uploaded_index_count {
        return Err(EngineError::GeometryRegister {
            detail: smol_str::format_smolstr!(
                "DrawIndexed range {first_index}..{end} exceeds the uploaded index count {uploaded_index_count}"
            ),
        });
    }
    Ok(DrawIndexedArgs {
        first_index,
        count,
        base_vertex: 0,
    })
}

impl GeometryDrawGpu {
    pub(crate) fn draw_indexed(
        &self,
        uploaded_index_count: u32,
    ) -> Result<DrawIndexedArgs, EngineError> {
        let count = self
            .range
            .end
            .checked_sub(self.range.start)
            .ok_or_else(|| EngineError::GeometryRegister {
                detail: smol_str::format_smolstr!(
                    "geometry {}:{} has a reversed draw range {}..{}",
                    self.id.index(),
                    self.id.generation(),
                    self.range.start,
                    self.range.end
                ),
            })?;
        draw_indexed_params(self.range.start, count, uploaded_index_count)
    }
}

impl InstanceBuffer {
    fn new() -> Self {
        let mut buffer = Self {
            instances: Vec::with_capacity(256),
            free: Vec::new(),
            used: 0,
            clips: Vec::with_capacity(8),
            geometry: Vec::with_capacity(8),
            ops: Vec::with_capacity(16),
            segments: Vec::new(),
            changed: ChangeSet {
                ranges: Vec::new(),
                sorted: true,
            },
        };
        buffer.clips.push(CLIP_SENTINEL);
        buffer
    }

    fn reset(&mut self) {
        self.instances.clear();
        self.free.clear();
        self.used = 0;
        self.clips.clear();
        self.clips.push(CLIP_SENTINEL);
        self.geometry.clear();
        self.ops.clear();
        self.segments.clear();
        self.changed.clear();
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn instance_kinds(&self) -> Vec<f32> {
        self.instances
            .iter()
            .map(|instance| instance.params1[2])
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.segments
            .iter()
            .flatten()
            .map(|segment| segment.len() as usize)
            .sum()
    }

    pub(crate) fn instance_count(&self) -> u32 {
        self.used
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn image_layers(&self) -> Vec<u16> {
        let mut layers = Vec::new();
        for segment in self.segments.iter().flatten() {
            for instance in &self.instances[segment.start as usize..segment.end as usize] {
                if instance.params1[2] == KIND_IMAGE {
                    layers.push(instance.params1[1] as u16);
                }
            }
        }
        layers.sort_unstable();
        layers
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.instances)
    }

    pub(crate) fn ops(&self) -> &[FrameOp] {
        &self.ops
    }

    pub(crate) fn geometry(&self) -> &[GeometryDrawGpu] {
        &self.geometry
    }

    pub(crate) fn clips(&self) -> &[ClipChain] {
        &self.clips
    }

    pub(crate) fn write_clips_gpu(&self, out: &mut Vec<GpuClipEntry>) {
        out.clear();
        out.extend(self.clips().iter().copied().map(ClipChain::gpu_entry));
    }

    pub(crate) fn changes(&mut self) -> &[std::ops::Range<u32>] {
        self.changed.merged()
    }

    pub(crate) fn clear_changes(&mut self) {
        self.changed.clear();
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn live_instances(&self) -> impl Iterator<Item = &[GpuPrimitive]> + '_ {
        let mut ranges: Vec<std::ops::Range<u32>> = self
            .segments
            .iter()
            .flatten()
            .filter(|segment| !segment.range().is_empty())
            .map(|segment| segment.range())
            .collect();
        ranges.sort_unstable_by_key(|range| range.start);
        ranges
            .into_iter()
            .map(move |range| &self.instances[range.start as usize..range.end as usize])
    }

    fn allocate(&mut self, len: u32) -> std::ops::Range<u32> {
        if len == 0 {
            return self.used..self.used;
        }
        for index in 0..self.free.len() {
            let range = self.free[index].clone();
            if range.end - range.start < len {
                continue;
            }
            let start = range.start;
            if range.end - start > len {
                self.free[index] = start + len..range.end;
            } else {
                self.free.remove(index);
            }
            return start..start + len;
        }
        let start = self.used;
        self.used += len;
        if self.instances.len() < self.used as usize {
            self.instances
                .resize(self.used as usize, bytemuck::Zeroable::zeroed());
        }
        start..start + len
    }

    fn release(&mut self, range: std::ops::Range<u32>) {
        if range.is_empty() {
            return;
        }
        let mut index = 0;
        while index < self.free.len() && self.free[index].start < range.start {
            index += 1;
        }
        self.free.insert(index, range);
        if index + 1 < self.free.len() && self.free[index].end == self.free[index + 1].start {
            let next = self.free.remove(index + 1);
            self.free[index].end = next.end;
        }
        if index > 0 && self.free[index - 1].end == self.free[index].start {
            let current = self.free.remove(index);
            self.free[index - 1].end = current.end;
        }
    }

    fn write_instances(&mut self, range: std::ops::Range<u32>, source: &[GpuPrimitive]) {
        debug_assert_eq!(range.len(), source.len());
        self.instances[range.start as usize..range.end as usize].copy_from_slice(source);
        self.changed.push(range);
    }

    fn needs_compaction(&self) -> bool {
        let free: u32 = self.free.iter().map(|range| range.end - range.start).sum();
        free >= 64 && free * 4 >= self.used
    }
}

#[derive(Clone, Copy, Debug)]
struct RoundedClip {
    rect: [f32; 4],
    corner_radius: [f32; 4],
    entry: ClipChainId,
}

#[derive(Clone, Debug)]
struct ClipState {
    rect_aabb: [f32; 4],
    has_rect: bool,
    rect_entry: Option<ClipChainId>,
    rounded: SmallVec<[RoundedClip; 8]>,
}

impl ClipState {
    fn unbounded() -> Self {
        Self {
            rect_aabb: [f32::MIN, f32::MIN, f32::MAX, f32::MAX],
            has_rect: false,
            rect_entry: None,
            rounded: SmallVec::new(),
        }
    }

    fn depth(&self) -> usize {
        self.rounded.len() + usize::from(self.rect_entry.is_some())
    }

    fn top(&self) -> Option<ClipChainId> {
        self.rounded
            .last()
            .map(|clip| clip.entry)
            .or(self.rect_entry)
    }

    fn duplicate_rounded(&self, rect: [f32; 4], corner_radius: [f32; 4]) -> bool {
        self.rounded.iter().any(|clip| {
            clip.rect.map(f32::to_bits) == rect.map(f32::to_bits)
                && clip.corner_radius.map(f32::to_bits) == corner_radius.map(f32::to_bits)
        })
    }

    fn aabb(&self) -> [f32; 4] {
        let mut aabb = self.rect_aabb;
        for clip in &self.rounded {
            aabb = intersect_aabb(aabb, rect_to_aabb(clip.rect));
        }
        aabb
    }
}

fn intersect_aabb(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

fn rect_to_aabb(rect: [f32; 4]) -> [f32; 4] {
    [rect[0], rect[1], rect[0] + rect[2], rect[1] + rect[3]]
}

fn aabb_to_rect(aabb: [f32; 4]) -> [f32; 4] {
    [aabb[0], aabb[1], aabb[2] - aabb[0], aabb[3] - aabb[1]]
}

fn transformed_aabb(transform: Transform, rect: LogicalRect) -> [f32; 4] {
    let far = LogicalPoint::new(
        rect.origin.x + rect.size.width,
        rect.origin.y + rect.size.height,
    );
    let corners = [
        transform.apply(rect.origin),
        transform.apply(LogicalPoint::new(far.x, rect.origin.y)),
        transform.apply(LogicalPoint::new(rect.origin.x, far.y)),
        transform.apply(far),
    ];
    if !corners
        .iter()
        .all(|point| point.x.is_finite() && point.y.is_finite())
    {
        return [1.0, 1.0, -1.0, -1.0];
    }
    let mut aabb = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for point in corners {
        aabb = [
            aabb[0].min(point.x.value()),
            aabb[1].min(point.y.value()),
            aabb[2].max(point.x.value()),
            aabb[3].max(point.y.value()),
        ];
    }
    aabb
}

fn visible_aabb(logical_aabb: [f32; 4], scale: DevicePixelRatio, clip_aabb: [f32; 4]) -> [f32; 4] {
    let factor = scale.factor();
    let scaled = [
        logical_aabb[0] * factor,
        logical_aabb[1] * factor,
        logical_aabb[2] * factor,
        logical_aabb[3] * factor,
    ];
    intersect_aabb(scaled, clip_aabb)
}

const CLIP_SENTINEL: ClipChain = ClipChain {
    parent: None,
    kind: ClipKind::Rect,
    rect: [0.0; 4],
    corner_radius: [0.0; 4],
};

pub(crate) struct InstanceEncoder {
    buffer: InstanceBuffer,
    state: ClipState,
    saved: SmallVec<[ClipState; 8]>,
    chains: ahash::HashMap<(u32, u32, [u32; 4], [u32; 4]), ClipChainId>,
    colors: crate::paint::color::ResolveCache,
    scale: Option<DevicePixelRatio>,
    scratch: Vec<GpuPrimitive>,
    scratch_parts: SmallVec<[LanePart; 2]>,
    encoded_lanes: usize,
    encoded_geometry_chunks: usize,
    written_bytes: usize,
    reused_segments: usize,
    primed: bool,
    #[cfg(test)]
    fast_encodes: u32,
    image_layers: u32,
}

impl InstanceEncoder {
    pub(crate) fn new() -> Self {
        Self {
            buffer: InstanceBuffer::new(),
            state: ClipState::unbounded(),
            saved: SmallVec::new(),
            chains: ahash::HashMap::default(),
            colors: crate::paint::color::ResolveCache::new(),
            scale: None,
            scratch: Vec::with_capacity(64),
            scratch_parts: SmallVec::new(),
            encoded_lanes: 0,
            encoded_geometry_chunks: 0,
            reused_segments: 0,
            written_bytes: 0,
            primed: false,
            image_layers: u32::MAX,
            #[cfg(test)]
            fast_encodes: 0,
        }
    }

    pub(crate) fn set_image_layers(&mut self, layers: u32) {
        self.image_layers = layers;
    }

    pub(crate) fn encode<S: LaneSource>(
        &mut self,
        source: &S,
        scale: DevicePixelRatio,
        resolver: ColorResolver,
        geometry: &dyn GeometryLookup,
    ) -> Result<&InstanceBuffer, EngineError> {
        self.encoded_lanes = 0;
        self.encoded_geometry_chunks = 0;
        self.reused_segments = 0;
        self.written_bytes = 0;
        self.colors.clear();
        let reset = self.scale != Some(scale) || self.buffer.clips.len() > CLIP_TABLE_LIMIT;
        if reset {
            self.buffer.reset();
            self.chains.clear();
            self.scale = Some(scale);
            self.primed = false;
        }
        let order = source.order();
        if !reset
            && self.primed
            && let Some(changed) = source.changed_lanes()
        {
            #[cfg(test)]
            {
                self.fast_encodes += 1;
            }
            self.encode_changed(source, changed, order, scale, resolver, geometry)?;
        } else {
            self.encode_scanned(source, order, scale, resolver, geometry)?;
            self.primed = true;
        }
        #[cfg(debug_assertions)]
        self.self_check(order);
        Ok(&self.buffer)
    }

    fn encode_scanned<S: LaneSource>(
        &mut self,
        source: &S,
        order: &[crate::paint::scene::LaneId],
        scale: DevicePixelRatio,
        resolver: ColorResolver,
        geometry: &dyn GeometryLookup,
    ) -> Result<(), EngineError> {
        let Self {
            buffer,
            state,
            saved,
            chains,
            colors,
            scratch,
            scratch_parts,
            image_layers,
            encoded_lanes,
            encoded_geometry_chunks,
            reused_segments,
            written_bytes,
            ..
        } = self;
        let mut ops = std::mem::take(&mut buffer.ops);
        ops.clear();
        let mut geometry_ops = std::mem::take(&mut buffer.geometry);
        geometry_ops.clear();
        let mut group = OpGroup::new();
        for lane_id in order {
            let slot = lane_id.slot() as usize;
            let stale = buffer.segments.get(slot).is_some_and(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|segment| segment.generation != lane_id.generation())
            });
            if stale && let Some(old) = buffer.segments[slot].take() {
                buffer.release(old.range());
            }
            let Some((version, primitives)) = source.lane_state(*lane_id) else {
                continue;
            };
            let scroll_slot = source.lane_scroll(*lane_id);
            let geometry_chunk = source.lane_geometry_chunk(*lane_id);
            let reusable = buffer
                .segments
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|segment| segment.version == version && segment.scroll == scroll_slot);
            if reusable {
                *reused_segments += 1;
            } else {
                scratch.clear();
                scratch_parts.clear();
                {
                    let mut lane = LaneEncoder {
                        instances: scratch,
                        parts: scratch_parts,
                        clips: &mut buffer.clips,
                        chains,
                        state,
                        saved,
                        scale,
                        resolver,
                        colors,
                        geometry,
                        chunk_scope: geometry_chunk,
                        emitted_geometry_chunks: 0,
                        run_start: 0,
                        image_layers: *image_layers,
                    };
                    lane.encode_lane(primitives, geometry_chunk)?;
                    *encoded_geometry_chunks += lane.emitted_geometry_chunks;
                }
                let len = scratch.len() as u32;
                let previous = buffer.segments.get_mut(slot).and_then(Option::take);
                let (mut segment, range) = match previous {
                    Some(old) if old.len() == len => {
                        let range = old.range();
                        buffer.write_instances(range.clone(), scratch);
                        (old, range)
                    }
                    Some(old) => {
                        buffer.release(old.range());
                        let range = buffer.allocate(len);
                        buffer.write_instances(range.clone(), scratch);
                        (old, range)
                    }
                    None => {
                        let range = buffer.allocate(len);
                        buffer.write_instances(range.clone(), scratch);
                        (
                            Box::new(LaneSegment {
                                generation: lane_id.generation(),
                                version,
                                scroll: scroll_slot,
                                start: 0,
                                end: 0,
                                parts: SmallVec::new(),
                            }),
                            range,
                        )
                    }
                };
                segment.generation = lane_id.generation();
                segment.version = version;
                segment.scroll = scroll_slot;
                segment.start = range.start;
                segment.end = range.start + len;
                segment.parts.clear();
                std::mem::swap(&mut segment.parts, scratch_parts);
                if buffer.segments.len() <= slot {
                    buffer.segments.resize_with(slot + 1, || None);
                }
                buffer.segments[slot] = Some(segment);
                *encoded_lanes += 1;
                *written_bytes += len as usize * 128;
            }
            if let Some(segment) = buffer.segments.get(slot).and_then(Option::as_deref)
                && segment.generation == lane_id.generation()
            {
                append_segment_ops(&mut ops, &mut geometry_ops, &mut group, segment);
            }
        }
        flush_group(&mut ops, &mut group);
        buffer.ops = ops;
        buffer.geometry = geometry_ops;
        if *encoded_lanes == 0 && *reused_segments > 0 && buffer.needs_compaction() {
            compact(buffer, order);
            build_ops(buffer, order);
        }
        Ok(())
    }

    fn encode_changed<S: LaneSource>(
        &mut self,
        source: &S,
        changed: &[crate::paint::scene::LaneId],
        order: &[crate::paint::scene::LaneId],
        scale: DevicePixelRatio,
        resolver: ColorResolver,
        geometry: &dyn GeometryLookup,
    ) -> Result<(), EngineError> {
        let mut ops_stale = false;
        {
            let Self {
                buffer,
                state,
                saved,
                chains,
                colors,
                scratch,
                scratch_parts,
                image_layers,
                encoded_lanes,
                encoded_geometry_chunks,
                written_bytes,
                ..
            } = self;
            for lane_id in changed {
                let slot = lane_id.slot() as usize;
                let stale = buffer.segments.get(slot).is_some_and(|entry| {
                    entry
                        .as_ref()
                        .is_some_and(|segment| segment.generation != lane_id.generation())
                });
                if stale && let Some(old) = buffer.segments[slot].take() {
                    buffer.release(old.range());
                    ops_stale = true;
                }
                let Some((version, primitives)) = source.lane_state(*lane_id) else {
                    continue;
                };
                let scroll_slot = source.lane_scroll(*lane_id);
                let geometry_chunk = source.lane_geometry_chunk(*lane_id);
                let reusable = buffer
                    .segments
                    .get(slot)
                    .and_then(Option::as_ref)
                    .is_some_and(|segment| {
                        segment.version == version && segment.scroll == scroll_slot
                    });
                if reusable {
                    continue;
                }
                let previous_range = buffer
                    .segments
                    .get(slot)
                    .and_then(Option::as_ref)
                    .map(|segment| segment.range());
                let previous_scroll = buffer
                    .segments
                    .get(slot)
                    .and_then(Option::as_ref)
                    .map(|segment| segment.scroll);
                scratch.clear();
                scratch_parts.clear();
                {
                    let mut lane = LaneEncoder {
                        instances: scratch,
                        parts: scratch_parts,
                        clips: &mut buffer.clips,
                        chains,
                        state,
                        saved,
                        scale,
                        resolver,
                        colors,
                        geometry,
                        chunk_scope: geometry_chunk,
                        emitted_geometry_chunks: 0,
                        run_start: 0,
                        image_layers: *image_layers,
                    };
                    lane.encode_lane(primitives, geometry_chunk)?;
                    *encoded_geometry_chunks += lane.emitted_geometry_chunks;
                }
                let parts_unchanged = buffer
                    .segments
                    .get(slot)
                    .and_then(Option::as_ref)
                    .is_some_and(|segment| parts_equal(&segment.parts, scratch_parts));
                ops_stale |= !parts_unchanged;
                ops_stale |= previous_scroll != Some(scroll_slot);
                let len = scratch.len() as u32;
                let previous = buffer.segments.get_mut(slot).and_then(Option::take);
                let (mut segment, range) = match previous {
                    Some(old) if old.len() == len => {
                        let range = old.range();
                        buffer.write_instances(range.clone(), scratch);
                        (old, range)
                    }
                    Some(old) => {
                        buffer.release(old.range());
                        let range = buffer.allocate(len);
                        buffer.write_instances(range.clone(), scratch);
                        (old, range)
                    }
                    None => {
                        let range = buffer.allocate(len);
                        buffer.write_instances(range.clone(), scratch);
                        (
                            Box::new(LaneSegment {
                                generation: lane_id.generation(),
                                version,
                                scroll: scroll_slot,
                                start: 0,
                                end: 0,
                                parts: SmallVec::new(),
                            }),
                            range,
                        )
                    }
                };
                segment.generation = lane_id.generation();
                segment.version = version;
                segment.scroll = scroll_slot;
                segment.start = range.start;
                segment.end = range.start + len;
                segment.parts.clear();
                std::mem::swap(&mut segment.parts, scratch_parts);
                if buffer.segments.len() <= slot {
                    buffer.segments.resize_with(slot + 1, || None);
                }
                buffer.segments[slot] = Some(segment);
                *encoded_lanes += 1;
                *written_bytes += len as usize * 128;
                ops_stale |= previous_range.as_ref() != Some(&range);
            }
        }
        if ops_stale {
            build_ops(&mut self.buffer, order);
        }
        if self.encoded_lanes == 0 && !order.is_empty() && self.buffer.needs_compaction() {
            compact(&mut self.buffer, order);
            build_ops(&mut self.buffer, order);
        }
        self.reused_segments = order.len().saturating_sub(self.encoded_lanes);
        Ok(())
    }

    pub(crate) fn release_lane(&mut self, lane: crate::paint::scene::LaneId) {
        let slot = lane.slot() as usize;
        let matches = self.buffer.segments.get(slot).is_some_and(|entry| {
            entry
                .as_ref()
                .is_some_and(|segment| segment.generation == lane.generation())
        });
        if matches && let Some(segment) = self.buffer.segments[slot].take() {
            self.buffer.release(segment.range());
        }
    }

    #[cfg(test)]
    pub(crate) fn fast_encodes(&self) -> u32 {
        self.fast_encodes
    }

    pub(crate) fn encoded_lanes(&self) -> usize {
        self.encoded_lanes
    }

    pub(crate) fn encoded_geometry_chunks(&self) -> usize {
        self.encoded_geometry_chunks
    }

    pub(crate) fn written_bytes(&self) -> usize {
        self.written_bytes
    }

    pub(crate) fn reused_segments(&self) -> usize {
        self.reused_segments
    }

    pub(crate) fn needs_reset(&self, scale: DevicePixelRatio) -> bool {
        self.scale != Some(scale) || self.buffer.clips.len() > CLIP_TABLE_LIMIT
    }

    pub(crate) fn invalidate(&mut self) {
        self.buffer.reset();
        self.chains.clear();
        self.state = ClipState::unbounded();
        self.saved.clear();
        self.scale = None;
        self.encoded_lanes = 0;
        self.encoded_geometry_chunks = 0;
        self.reused_segments = 0;
        self.written_bytes = 0;
        self.primed = false;
    }

    pub(crate) fn buffer(&self) -> &InstanceBuffer {
        &self.buffer
    }

    pub(crate) fn buffer_mut(&mut self) -> &mut InstanceBuffer {
        &mut self.buffer
    }

    #[cfg(debug_assertions)]
    fn self_check(&self, order: &[crate::paint::scene::LaneId]) {
        let buffer = &self.buffer;
        let mut covered = 0usize;
        for op in &buffer.ops {
            match *op {
                FrameOp::Instances { start, len, .. } => {
                    covered += len as usize;
                    debug_assert!(len > 0, "FrameOp::Instances must not be empty");
                    debug_assert!(
                        (start as usize) + (len as usize) <= buffer.instances.len(),
                        "FrameOp::Instances out of range"
                    );
                }
                FrameOp::Geometry {
                    index,
                    clip_index,
                    clip_rect,
                    ..
                } => {
                    debug_assert!(
                        (index as usize) < buffer.geometry.len(),
                        "FrameOp::Geometry out of range"
                    );
                    debug_assert!(
                        (clip_index as usize) < buffer.clips.len(),
                        "FrameOp::Geometry clip out of range"
                    );
                    debug_assert!(
                        clip_rect
                            .iter()
                            .all(|bits| f32::from_bits(*bits).is_finite()),
                        "FrameOp::Geometry clip rect must be finite"
                    );
                }
            }
        }
        for draw in &buffer.geometry {
            debug_assert!(
                draw.range.start <= draw.range.end,
                "geometry draw range must be a half-open interval"
            );
            debug_assert!(
                draw.range.is_empty() || draw.range.end <= draw.snapshot.index_count(),
                "geometry draw range must stay inside the snapshot index buffer"
            );
        }
        let mut live = 0usize;
        let mut ranges: Vec<std::ops::Range<u32>> = Vec::new();
        for (slot, entry) in buffer.segments.iter().enumerate() {
            let Some(segment) = entry.as_deref() else {
                continue;
            };
            debug_assert_eq!(
                segment.end,
                segment.start + segment.len(),
                "segment {slot} range mismatch"
            );
            debug_assert!(
                (segment.end as usize) <= buffer.instances.len(),
                "segment {slot} escapes the instance arena"
            );
            let parts_len: u32 = segment
                .parts
                .iter()
                .map(|part| match part {
                    LanePart::Instances { len, .. } => *len,
                    LanePart::Geometry(_) => 0,
                })
                .sum();
            debug_assert_eq!(
                parts_len,
                segment.len(),
                "segment {slot} parts disagree with its length"
            );
            if !segment.range().is_empty() {
                ranges.push(segment.range());
            }
        }
        for lane_id in order {
            let slot = lane_id.slot() as usize;
            if let Some(segment) = buffer.segments.get(slot).and_then(Option::as_deref)
                && segment.generation == lane_id.generation()
            {
                live += segment.len() as usize;
            }
        }
        debug_assert_eq!(covered, live, "FrameOp must cover every live instance");
        ranges.sort_unstable_by_key(|range| range.start);
        for pair in ranges.windows(2) {
            debug_assert!(
                pair[0].end <= pair[1].start,
                "instance segments must not overlap"
            );
        }
        for (index, chain) in buffer.clips.iter().enumerate() {
            debug_assert!(
                chain.rect.iter().all(|value| value.is_finite()),
                "clip rect must be finite"
            );
            debug_assert!(
                chain.corner_radius.iter().all(|value| value.is_finite()),
                "clip corner radius must be finite"
            );
            if let Some(parent) = chain.parent {
                debug_assert!(
                    parent.index() < index,
                    "clip chain parent must precede its child"
                );
            }
            let entry = chain.gpu_entry();
            let rect_kind = chain.corner_radius.iter().all(|radius| *radius == 0.0);
            let expected = if rect_kind {
                CLIP_KIND_RECT as u32
            } else {
                CLIP_KIND_ROUNDED as u32
            };
            debug_assert_eq!(
                entry.meta[1], expected,
                "GpuClipEntry.meta.y 必须与覆盖率语义一致（1 = 矩形 scissor、2 = 圆角 AA）"
            );
            debug_assert_eq!(entry.meta[2], 0, "ClipEntry.meta.zw 保留恒 0");
            debug_assert_eq!(entry.meta[3], 0, "ClipEntry.meta.zw 保留恒 0");
        }
        for segment in buffer.segments.iter().flatten() {
            for primitive in &buffer.instances[segment.start as usize..segment.end as usize] {
                let clip_index = primitive.params1[0] as usize;
                debug_assert!(
                    clip_index < buffer.clips.len(),
                    "instance clip index out of range"
                );
                let mut depth = 0usize;
                let mut rects = 0usize;
                let mut cursor = clip_index;
                while cursor != 0 {
                    depth += 1;
                    debug_assert!(
                        depth <= MAX_CLIP_ITEMS as usize,
                        "clip chain longer than MAX_CLIP_ITEMS"
                    );
                    let entry = buffer.clips[cursor];
                    if entry.kind == ClipKind::Rect {
                        rects += 1;
                        debug_assert_eq!(entry.parent, None, "矩形项必须是链根");
                    }
                    cursor = entry.parent.map_or(0, |parent| parent.index());
                }
                debug_assert!(rects <= 1, "链上至多一条矩形 clip 项");
                debug_assert_eq!(
                    depth, primitive.params1[3] as usize,
                    "instance clip_depth must match the chain length"
                );
            }
        }
    }
}

struct OpGroup {
    active: bool,
    start: u32,
    len: u32,
    lanes: u32,
    bounds: DeviceRect,
    scroll: u32,
}

impl OpGroup {
    fn new() -> Self {
        Self {
            active: false,
            start: 0,
            len: 0,
            lanes: 0,
            bounds: DeviceRect::default(),
            scroll: 0,
        }
    }
}

fn append_segment_ops(
    ops: &mut Vec<FrameOp>,
    geometry: &mut Vec<GeometryDrawGpu>,
    group: &mut OpGroup,
    segment: &LaneSegment,
) {
    let mut cursor = segment.start;
    let mut lane_counted = false;
    for part in &segment.parts {
        match part {
            LanePart::Instances { len, bounds } => {
                if *len == 0 {
                    continue;
                }
                let contiguous = group.active
                    && group.start + group.len == cursor
                    && group.lanes < MAX_LANES_PER_OP
                    && group.scroll == segment.scroll;
                if group.active && !contiguous {
                    flush_group(ops, group);
                }
                if !group.active {
                    *group = OpGroup {
                        active: true,
                        start: cursor,
                        scroll: segment.scroll,
                        ..OpGroup::new()
                    };
                }
                if !lane_counted {
                    group.lanes += 1;
                    lane_counted = true;
                }
                group.len += *len;
                group.bounds = group.bounds.union(*bounds);
                cursor += *len;
            }
            LanePart::Geometry(part) => {
                flush_group(ops, group);
                let index = geometry.len() as u32;
                geometry.push(part.draw.clone());
                ops.push(FrameOp::Geometry {
                    index,
                    clip_index: part.clip_index,
                    clip_rect: part.clip_rect,
                    bounds: part.bounds,
                });
            }
        }
    }
}

fn build_ops(buffer: &mut InstanceBuffer, order: &[crate::paint::scene::LaneId]) {
    let mut ops = std::mem::take(&mut buffer.ops);
    ops.clear();
    let mut geometry = std::mem::take(&mut buffer.geometry);
    geometry.clear();
    let mut group = OpGroup::new();
    for lane_id in order {
        let slot = lane_id.slot() as usize;
        let Some(segment) = buffer.segments.get(slot).and_then(Option::as_deref) else {
            continue;
        };
        if segment.generation != lane_id.generation() {
            continue;
        }
        append_segment_ops(&mut ops, &mut geometry, &mut group, segment);
    }
    flush_group(&mut ops, &mut group);
    buffer.ops = ops;
    buffer.geometry = geometry;
}

fn parts_equal(previous: &[LanePart], current: &[LanePart]) -> bool {
    previous.len() == current.len()
        && previous
            .iter()
            .zip(current)
            .all(|(old, new)| match (old, new) {
                (
                    LanePart::Instances {
                        len: old_len,
                        bounds: old_bounds,
                    },
                    LanePart::Instances {
                        len: new_len,
                        bounds: new_bounds,
                    },
                ) => old_len == new_len && old_bounds == new_bounds,
                _ => false,
            })
}

fn flush_group(ops: &mut Vec<FrameOp>, group: &mut OpGroup) {
    if group.active && group.len > 0 {
        ops.push(FrameOp::Instances {
            start: group.start,
            len: group.len,
            bounds: group.bounds,
            scroll: group.scroll,
        });
    }
    group.active = false;
    group.len = 0;
    group.lanes = 0;
}

fn compact(buffer: &mut InstanceBuffer, order: &[crate::paint::scene::LaneId]) {
    let mut packed: Vec<GpuPrimitive> = Vec::with_capacity(buffer.instances.len());
    let mut starts: Vec<(usize, u32)> = Vec::with_capacity(order.len());
    let mut seen: Vec<bool> = vec![false; buffer.segments.len()];
    let mut cursor = 0u32;
    for lane_id in order {
        let slot = lane_id.slot() as usize;
        let Some(segment) = buffer.segments.get(slot).and_then(Option::as_deref) else {
            continue;
        };
        if segment.generation != lane_id.generation() {
            continue;
        }
        let range = segment.range();
        packed.extend_from_slice(&buffer.instances[range.start as usize..range.end as usize]);
        starts.push((slot, cursor));
        seen[slot] = true;
        cursor += segment.len();
    }
    for (live, entry) in seen.iter().zip(buffer.segments.iter_mut()) {
        if !*live {
            *entry = None;
        }
    }
    buffer.instances.clear();
    buffer.instances.extend_from_slice(&packed);
    buffer.used = cursor;
    buffer
        .instances
        .resize(buffer.used as usize, bytemuck::Zeroable::zeroed());
    buffer.free.clear();
    for (slot, start) in starts {
        if let Some(segment) = buffer.segments[slot].as_mut() {
            let len = segment.len();
            segment.start = start;
            segment.end = start + len;
        }
    }
    buffer.changed.clear();
    buffer.changed.push(0..cursor);
}

struct LaneEncoder<'a> {
    instances: &'a mut Vec<GpuPrimitive>,
    parts: &'a mut SmallVec<[LanePart; 2]>,
    clips: &'a mut Vec<ClipChain>,
    state: &'a mut ClipState,
    saved: &'a mut SmallVec<[ClipState; 8]>,
    chains: &'a mut ahash::HashMap<(u32, u32, [u32; 4], [u32; 4]), ClipChainId>,
    scale: DevicePixelRatio,
    resolver: ColorResolver,
    colors: &'a mut crate::paint::color::ResolveCache,
    geometry: &'a dyn GeometryLookup,
    chunk_scope: Option<(crate::paint::geometry::GeometryId, u32)>,
    emitted_geometry_chunks: usize,
    run_start: usize,
    image_layers: u32,
}

impl LaneEncoder<'_> {
    fn encode_lane(
        &mut self,
        primitives: &[Primitive],
        chunk_scope: Option<(crate::paint::geometry::GeometryId, u32)>,
    ) -> Result<(), EngineError> {
        self.chunk_scope = chunk_scope;
        *self.state = ClipState::unbounded();
        self.saved.clear();
        for primitive in primitives {
            self.encode_primitive(primitive)?;
        }
        debug_assert_eq!(self.state.depth(), 0, "lane must be clip-balanced (C1)");
        self.flush_run();
        Ok(())
    }

    fn encode_primitive(&mut self, primitive: &Primitive) -> Result<(), EngineError> {
        match primitive {
            Primitive::Clip(ClipOp::Push(rect)) => self.push_clip(*rect)?,
            Primitive::Clip(ClipOp::Pop) => self.pop_clip(),
            Primitive::Quad(quad) => {
                if quad_visible(quad) {
                    self.emit_quad(quad);
                }
            }
            Primitive::Shadow(shadow) => {
                if !shadow.color.is_transparent() {
                    self.emit_shadow(shadow);
                }
            }
            Primitive::Image(image) => {
                if !image.modulation.is_transparent() && !image.bounds.is_empty() {
                    self.emit_image(image);
                }
            }
            Primitive::GlyphRun(run) => {
                if !run.color.is_transparent() {
                    let color0 = self.colors.resolve(&self.resolver, run.color);
                    for glyph in &run.glyphs {
                        self.emit_glyph(color0, glyph);
                    }
                }
            }
            Primitive::Text(_) => {}
            Primitive::Path(path) => {
                self.flush_run();
                let color = self.colors.resolve(&self.resolver, path.fill).to_array();
                self.push_geometry_op(
                    path.geometry,
                    Transform::IDENTITY,
                    color,
                    crate::paint::geometry::FULL_INDEX_RANGE,
                    Some(path.bounds),
                );
            }
            Primitive::Geometry(draw) => {
                self.flush_run();
                let color = self.colors.resolve(&self.resolver, draw.color).to_array();
                self.push_geometry_op(draw.id, draw.transform, color, draw.range.clone(), None);
            }
            Primitive::Canvas(canvas) => {
                let entry_depth = self.state.depth();
                for inner in canvas.primitives() {
                    self.encode_primitive(inner)?;
                }
                debug_assert_eq!(
                    self.state.depth(),
                    entry_depth,
                    "Canvas must be clip-balanced"
                );
            }
        }
        Ok(())
    }

    fn push_clip(&mut self, clip: crate::paint::primitive::ClipRect) -> Result<(), EngineError> {
        self.saved.push(self.state.clone());
        let factor = self.scale.factor();
        let logical = clip.rect;
        let rect = if logical.is_finite()
            && logical.size.width > LogicalPx::ZERO
            && logical.size.height > LogicalPx::ZERO
        {
            device_bounds(self.scale.to_covering_device_rect(logical))
        } else {
            let x = if logical.origin.x.is_finite() {
                (logical.origin.x.value() * factor).floor()
            } else {
                0.0
            };
            let y = if logical.origin.y.is_finite() {
                (logical.origin.y.value() * factor).floor()
            } else {
                0.0
            };
            [x, y, 0.0, 0.0]
        };
        let radii = crate::math::normalize_physical_corner_radius(
            clip.corner_radius
                .map(|radius| self.scale.to_physical(radius)),
            PhysicalPx::new(rect[2]),
            PhysicalPx::new(rect[3]),
        )
        .map(f32::from);
        if radii.iter().all(|radius| *radius == 0.0) {
            let aabb = rect_to_aabb(rect);
            self.state.rect_aabb = if self.state.has_rect {
                intersect_aabb(self.state.rect_aabb, aabb)
            } else {
                aabb
            };
            self.state.has_rect = true;
            self.rebuild_rect_chain();
        } else if !self.state.duplicate_rounded(rect, radii) {
            let parent = self.state.top();
            let entry = self.chain_entry(parent, ClipKind::Rounded, rect, radii);
            self.state.rounded.push(RoundedClip {
                rect,
                corner_radius: radii,
                entry,
            });
        }
        let rounded_depth = self.state.rounded.len();
        if rounded_depth > usize::from(MAX_CLIP_DEPTH) {
            return Err(EngineError::ClipDepthExceeded {
                depth: rounded_depth as u8,
                limit: MAX_CLIP_DEPTH,
            });
        }
        Ok(())
    }

    fn pop_clip(&mut self) {
        *self.state = self.saved.pop().unwrap_or_else(ClipState::unbounded);
    }

    fn rebuild_rect_chain(&mut self) {
        let rect_entry = if self.state.has_rect {
            let rect = aabb_to_rect(self.state.rect_aabb);
            debug_assert!(
                rect.iter().all(|value| value.fract() == 0.0),
                "矩形 clip 链的每条边必须是整数物理像素：{rect:?}"
            );
            Some(self.chain_entry(None, ClipKind::Rect, rect, [0.0; 4]))
        } else {
            None
        };
        self.state.rect_entry = rect_entry;
        let mut parent = rect_entry;
        for index in 0..self.state.rounded.len() {
            let (rect, radii) = {
                let clip = &self.state.rounded[index];
                (clip.rect, clip.corner_radius)
            };
            let entry = self.chain_entry(parent, ClipKind::Rounded, rect, radii);
            self.state.rounded[index].entry = entry;
            parent = Some(entry);
        }
    }

    fn chain_entry(
        &mut self,
        parent: Option<ClipChainId>,
        kind: ClipKind,
        rect: [f32; 4],
        corner_radius: [f32; 4],
    ) -> ClipChainId {
        debug_assert!(
            rect.iter().all(|value| value.fract() == 0.0),
            "clip 链的物理边界必须是整数：{rect:?}"
        );
        let key = (
            parent.map_or(0, |parent| parent.0),
            kind.gpu(),
            rect.map(f32::to_bits),
            corner_radius.map(f32::to_bits),
        );
        if let Some(existing) = self.chains.get(&key) {
            return *existing;
        }
        let id = ClipChainId(self.clips.len() as u32);
        self.clips.push(ClipChain {
            parent,
            kind,
            rect,
            corner_radius,
        });
        self.chains.insert(key, id);
        id
    }

    fn clip_index(&self) -> f32 {
        self.state.top().map_or(0, |id| id.0) as f32
    }

    fn clip_depth(&self) -> f32 {
        self.state.depth() as f32
    }

    fn clip_aabb(&self) -> [f32; 4] {
        self.state.aabb()
    }

    fn clip_rect(&self) -> [f32; 4] {
        if self.state.top().is_none() {
            return [f32::MIN, f32::MIN, f32::MAX, f32::MAX];
        }
        aabb_to_rect(self.clip_aabb())
    }

    fn flush_run(&mut self) {
        let len = self.instances.len() - self.run_start;
        if len > 0 {
            let bounds = run_bounds(&self.instances[self.run_start..]);
            self.parts.push(LanePart::Instances {
                len: len as u32,
                bounds,
            });
            self.run_start = self.instances.len();
        }
    }

    fn geometry_snapshot(
        &self,
        id: crate::paint::geometry::GeometryId,
    ) -> Option<std::sync::Arc<crate::paint::geometry::GeometrySnapshot>> {
        self.geometry.snapshot_arc(id)
    }

    fn push_geometry_op(
        &mut self,
        id: crate::paint::geometry::GeometryId,
        transform: Transform,
        color: [f32; 4],
        range: std::ops::Range<u32>,
        op_bounds: Option<LogicalRect>,
    ) {
        let registered = self.geometry_snapshot(id);
        let clip_aabb = self.clip_aabb();
        let clip_index = self.clip_index() as u32;
        let clip_rect = clip_aabb.map(f32::to_bits);
        let scope = self.chunk_scope.filter(|(scoped, _)| *scoped == id);
        let mut emitted = 0usize;
        let mut push_part = |emitted: &mut usize,
                             clipped: std::ops::Range<u32>,
                             bounds: LogicalRect,
                             snapshot: std::sync::Arc<_>| {
            let visible = visible_aabb(transformed_aabb(transform, bounds), self.scale, clip_aabb);
            self.parts.push(LanePart::Geometry(Box::new(LaneGeometry {
                draw: GeometryDrawGpu {
                    id,
                    range: clipped,
                    transform,
                    color,
                    snapshot,
                },
                clip_index,
                clip_rect,
                bounds: bounds_of_aabb(visible),
            })));
            *emitted += 1;
        };
        match (&registered, scope) {
            (Some(snapshot), Some((_, chunk))) => {
                if let Some(meta) = snapshot.chunk(chunk)
                    && let Some(clipped) = crate::paint::geometry::chunk_draw_range(
                        snapshot.topology(),
                        snapshot.index_count(),
                        meta,
                        range,
                    )
                {
                    let local = match (op_bounds, meta.bounds.is_empty()) {
                        (Some(bounds), _) => bounds,
                        (None, false) => meta.bounds,
                        (None, true) => snapshot.bounds(),
                    };
                    push_part(
                        &mut emitted,
                        clipped,
                        local,
                        std::sync::Arc::clone(snapshot),
                    );
                }
            }
            (Some(snapshot), None) => {
                for meta in snapshot.chunks() {
                    let Some(clipped) = crate::paint::geometry::chunk_draw_range(
                        snapshot.topology(),
                        snapshot.index_count(),
                        meta,
                        range.clone(),
                    ) else {
                        continue;
                    };
                    let local = match (op_bounds, meta.bounds.is_empty()) {
                        (Some(bounds), _) => bounds,
                        (None, false) => meta.bounds,
                        (None, true) => snapshot.bounds(),
                    };
                    push_part(
                        &mut emitted,
                        clipped,
                        local,
                        std::sync::Arc::clone(snapshot),
                    );
                }
            }
            (None, _) => push_part(
                &mut emitted,
                0..0,
                op_bounds.unwrap_or_else(|| {
                    LogicalRect::new(
                        px(clip_aabb[0]),
                        px(clip_aabb[1]),
                        px((clip_aabb[2] - clip_aabb[0]).max(0.0)),
                        px((clip_aabb[3] - clip_aabb[1]).max(0.0)),
                    )
                }),
                crate::paint::geometry_store::missing_snapshot(),
            ),
        }
        self.emitted_geometry_chunks += emitted;
    }

    fn emit_quad(&mut self, quad: &crate::paint::primitive::Quad) {
        let bounds = device_bounds(self.scale.to_device_rect(quad.bounds));
        if bounds[2] <= 0.0 || bounds[3] <= 0.0 {
            return;
        }
        let radii = crate::math::normalize_physical_corner_radius(
            quad.corner_radius
                .map(|radius| self.scale.to_physical(radius)),
            PhysicalPx::new(bounds[2]),
            PhysicalPx::new(bounds[3]),
        )
        .map(f32::from);
        let short_side = bounds[2].min(bounds[3]);
        let (color0, color1, border_width, gradient) = match &quad.paint {
            QuadPaint::Solid { fill, border } => {
                let width =
                    border.map_or(0.0, |border| self.scale.stroke_width(border.width).value());
                (
                    self.colors.resolve(&self.resolver, *fill),
                    border.map_or(LinearPremulRgba::TRANSPARENT, |border| {
                        self.colors.resolve(&self.resolver, border.color)
                    }),
                    width.clamp(0.0, short_side),
                    GRADIENT_NONE,
                )
            }
            QuadPaint::Gradient(gradient) => (
                self.colors.resolve(&self.resolver, gradient.from),
                self.colors.resolve(&self.resolver, gradient.to),
                0.0,
                match gradient.kind {
                    GradientKind::Horizontal => GRADIENT_HORIZONTAL,
                    GradientKind::Vertical => GRADIENT_VERTICAL,
                    GradientKind::Radial => GRADIENT_RADIAL,
                },
            ),
        };
        let instance = GpuPrimitive {
            bounds,
            corner_radius: radii,
            color0: color0.to_array(),
            color1: color1.to_array(),
            params0: [border_width, 0.0, gradient, 1.0],
            params1: [self.clip_index(), 0.0, KIND_QUAD, self.clip_depth()],
            uv: [0.0; 4],
            clip_rect: self.clip_rect(),
        };
        self.instances.push(instance);
    }

    fn emit_shadow(&mut self, shadow: &crate::paint::primitive::Shadow) {
        let bounds = self
            .scale
            .to_physical_rect(shadow.rect.translate(shadow.offset))
            .to_aabb();
        let blur = self.scale.to_physical(shadow.blur.max(LogicalPx::ZERO));
        let radii = crate::math::normalize_physical_corner_radius(
            shadow
                .corner_radius
                .map(|radius| self.scale.to_physical(radius)),
            PhysicalPx::new(bounds[2]),
            PhysicalPx::new(bounds[3]),
        )
        .map(f32::from);
        let sigma = (blur.value() * SHADOW_SIGMA_SCALE).max(SHADOW_SIGMA_MIN);
        let pad = SHADOW_PAD_SIGMAS * sigma + 1.0;
        let instance = GpuPrimitive {
            bounds,
            corner_radius: radii,
            color0: self.colors.resolve(&self.resolver, shadow.color).to_array(),
            color1: [0.0; 4],
            params0: [0.0, blur.value(), GRADIENT_NONE, pad],
            params1: [self.clip_index(), 0.0, KIND_SHADOW, self.clip_depth()],
            uv: [0.0; 4],
            clip_rect: self.clip_rect(),
        };
        self.instances.push(instance);
    }

    fn emit_image(&mut self, image: &crate::paint::primitive::Image) {
        if image.bounds.is_empty() {
            return;
        }
        debug_assert!(
            image.tile.page < MAX_IMAGE_PAGES,
            "image tile page {} out of range (MAX_IMAGE_PAGES={MAX_IMAGE_PAGES})",
            image.tile.page
        );
        if u32::from(image.tile.page) >= self.image_layers {
            return;
        }
        let page = image.tile.page.min(MAX_IMAGE_PAGES - 1);
        let bounds = device_bounds(self.scale.to_device_rect(image.bounds));
        let color0 = self.colors.resolve(&self.resolver, image.modulation);
        let instance = GpuPrimitive {
            bounds,
            corner_radius: [0.0; 4],
            color0: color0.to_array(),
            color1: [0.0; 4],
            params0: [0.0; 4],
            params1: [
                self.clip_index(),
                f32::from(page),
                KIND_IMAGE,
                self.clip_depth(),
            ],
            uv: image.tile.uv(),
            clip_rect: self.clip_rect(),
        };
        self.instances.push(instance);
    }

    fn emit_glyph(
        &mut self,
        color0: LinearPremulRgba,
        glyph: &crate::paint::primitive::PositionedGlyph,
    ) {
        debug_assert_eq!(
            glyph.tile.page, 0,
            "glyph atlas tile must live in texture array layer 0"
        );
        debug_assert!(
            glyph.tile.atlas.is_glyph(),
            "glyph run tile must come from a glyph atlas"
        );
        if glyph.size.is_empty() {
            return;
        }
        let bounds = device_bounds(
            self.scale
                .to_device_rect(LogicalRect::from_origin_size(glyph.position, glyph.size)),
        );
        let kind = if glyph.tile.atlas == crate::paint::atlas::AtlasKind::ColorGlyph {
            KIND_COLOR_GLYPH
        } else {
            KIND_GLYPH
        };
        let instance = GpuPrimitive {
            bounds,
            corner_radius: [0.0; 4],
            color0: color0.to_array(),
            color1: [0.0; 4],
            params0: [0.0; 4],
            params1: [self.clip_index(), 0.0, kind, self.clip_depth()],
            uv: glyph.tile.uv(),
            clip_rect: self.clip_rect(),
        };
        self.instances.push(instance);
    }
}

fn device_bounds(rect: crate::math::DeviceRect) -> [f32; 4] {
    [
        f32::from(rect.x0),
        f32::from(rect.y0),
        f32::from(rect.width()),
        f32::from(rect.height()),
    ]
}

fn quad_visible(quad: &crate::paint::primitive::Quad) -> bool {
    if quad.bounds.is_empty() {
        return false;
    }
    match &quad.paint {
        QuadPaint::Solid { fill, border } => {
            !fill.is_transparent() || border.is_some_and(|border| !border.color.is_transparent())
        }
        QuadPaint::Gradient(gradient) => {
            !(gradient.from.is_transparent() && gradient.to.is_transparent())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        CLIP_KIND_RECT, CLIP_KIND_ROUNDED, ClipChain, ClipChainId, ClipKind, ClipState,
        DrawIndexedArgs, GRADIENT_HORIZONTAL, GRADIENT_RADIAL, GRADIENT_VERTICAL, GpuPrimitive,
        InstanceEncoder, KIND_COLOR_GLYPH, KIND_GLYPH, KIND_QUAD, KIND_SHADOW, MAX_CLIP_ITEMS,
        SHADOW_SIGMA_MIN, SHADOW_SIGMA_SCALE, draw_indexed_params,
    };
    use crate::{
        error::EngineError,
        math::{
            DevicePixelRatio, LogicalPoint, LogicalRect, LogicalSize, PhysicalPx, Transform, dp, px,
        },
        paint::{
            atlas::{AtlasKind, AtlasTile},
            canvas::Canvas,
            color::{Color, ColorResolver},
            geometry::GeometryId,
            primitive::{
                Border, ClipOp, ClipRect, GeometryDraw, GlyphRun, Gradient, GradientKind, Image,
                PositionedGlyph, Primitive, Quad, QuadPaint, Shadow,
            },
            scene::{MAX_CLIP_DEPTH, Scene},
        },
    };

    fn scale() -> DevicePixelRatio {
        DevicePixelRatio::new(2.0).expect("合法 scale")
    }

    #[test]
    fn the_scrolled_scissor_narrows_only_real_scroll_slots() {
        let clips = [[0, 0, 0, 0], [10, 20, 110, 220]];
        assert_eq!(
            super::scrolled_scissor(&clips, 0, [0, 0, 800, 600]),
            None,
            "槽位 0 是「不随滚动偏移」的哨兵，不得收窄"
        );
        assert_eq!(
            super::scrolled_scissor(&clips, 1, [50, 0, 800, 600]),
            Some([50, 20, 110, 220]),
            "帧 scissor 与槽位屏幕裁剪区求交"
        );
        assert_eq!(
            super::scrolled_scissor(&clips, 1, [0, 300, 800, 600]),
            Some([10, 300, 110, 220]),
            "完全不相交 ⇒ 空区间（调用方跳过整个 op）"
        );
        assert_eq!(
            super::scrolled_scissor(&clips, 9, [0, 0, 800, 600]),
            None,
            "越界槽位按不收窄处理（保守：与不滚动等价）"
        );
    }

    fn resolver() -> ColorResolver {
        ColorResolver
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> LogicalRect {
        LogicalRect::new(px(x), px(y), px(w), px(h))
    }

    fn quad_primitive(paint: QuadPaint) -> Primitive {
        Primitive::Quad(Quad {
            bounds: rect(1.0, 2.0, 10.0, 20.0),
            corner_radius: [px(1.0), px(2.0), px(3.0), px(4.0)],
            paint,
        })
    }

    fn solid() -> QuadPaint {
        QuadPaint::Solid {
            fill: Color::WHITE,
            border: None,
        }
    }

    struct Encoded {
        instances: Vec<GpuPrimitive>,
        ops: Vec<super::FrameOp>,
        clips: Vec<ClipChain>,
    }

    impl Encoded {
        fn write_clips_gpu(&self, out: &mut Vec<super::GpuClipEntry>) {
            out.clear();
            out.extend(self.clips.iter().copied().map(ClipChain::gpu_entry));
        }
    }

    fn encode(scene: &Scene) -> Encoded {
        try_encode(scene).expect("encode")
    }

    fn encode_at(scene: &Scene, factor: f32) -> Encoded {
        let mut encoder = InstanceEncoder::new();
        let scale = DevicePixelRatio::new(factor).expect("合法 scale");
        let buffer = encoder
            .encode(
                scene,
                scale,
                resolver(),
                &crate::paint::geometry_store::GeometryStore::new(),
            )
            .expect("encode");
        Encoded {
            instances: buffer.instances.clone(),
            clips: buffer.clips.clone(),
            ops: buffer.ops.clone(),
        }
    }

    fn try_encode(scene: &Scene) -> Result<Encoded, crate::error::EngineError> {
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(
            scene,
            scale(),
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )?;
        Ok(Encoded {
            instances: buffer.instances.clone(),
            clips: buffer.clips.clone(),
            ops: buffer.ops.clone(),
        })
    }

    fn scene_with(lane_primitives: impl FnOnce(&mut Scene, crate::paint::scene::LaneId)) -> Scene {
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        lane_primitives(&mut scene, lane);
        scene.set_order(&[lane]);
        scene
    }

    fn push_clip(scene: &mut Scene, lane: crate::paint::scene::LaneId, clip: ClipRect) {
        assert!(scene.push(lane, Primitive::Clip(ClipOp::Push(clip))));
    }

    fn clip_coverage(clips: &[ClipChain], head: ClipChainId, pixel: (f32, f32)) -> f32 {
        let mut m = 1.0f32;
        let mut cursor = Some(head);
        while let Some(id) = cursor {
            let entry = clips[id.index()];
            if entry.rect[2] <= 0.0 || entry.rect[3] <= 0.0 {
                return 0.0;
            }
            match entry.kind {
                ClipKind::Rect => {
                    let (pixel_x, pixel_y) = (pixel.0.floor(), pixel.1.floor());
                    let (lo_x, lo_y) = (entry.rect[0], entry.rect[1]);
                    let (hi_x, hi_y) = (lo_x + entry.rect[2], lo_y + entry.rect[3]);
                    if pixel_x < lo_x || pixel_y < lo_y || pixel_x >= hi_x || pixel_y >= hi_y {
                        return 0.0;
                    }
                }
                ClipKind::Rounded => {
                    let half = (entry.rect[2] * 0.5, entry.rect[3] * 0.5);
                    let center = (entry.rect[0] + half.0, entry.rect[1] + half.1);
                    let d = sd_rounded_rect(
                        (pixel.0 - center.0, pixel.1 - center.1),
                        half,
                        entry.corner_radius,
                    );
                    m *= (0.5 - d).clamp(0.0, 1.0);
                }
            }
            cursor = entry.parent;
        }
        m
    }

    fn sd_rounded_rect(p: (f32, f32), half: (f32, f32), radii: [f32; 4]) -> f32 {
        let r = if p.0 > 0.0 {
            if p.1 > 0.0 { radii[2] } else { radii[1] }
        } else if p.1 > 0.0 {
            radii[3]
        } else {
            radii[0]
        };
        let r = r.clamp(0.0, half.0.min(half.1));
        let q = (p.0.abs() - half.0 + r, p.1.abs() - half.1 + r);
        let outside = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt();
        q.0.max(q.1).min(0.0) + outside - r
    }

    #[test]
    fn layout_is_128_by_16() {
        use std::mem::{align_of, offset_of, size_of};

        assert_eq!(size_of::<GpuPrimitive>(), 128);
        assert_eq!(align_of::<GpuPrimitive>(), 16);
        assert_eq!(offset_of!(GpuPrimitive, bounds), 0);
        assert_eq!(offset_of!(GpuPrimitive, corner_radius), 16);
        assert_eq!(offset_of!(GpuPrimitive, color0), 32);
        assert_eq!(offset_of!(GpuPrimitive, color1), 48);
        assert_eq!(offset_of!(GpuPrimitive, params0), 64);
        assert_eq!(offset_of!(GpuPrimitive, params1), 80);
        assert_eq!(offset_of!(GpuPrimitive, uv), 96);
        assert_eq!(offset_of!(GpuPrimitive, clip_rect), 112);
    }

    #[test]
    fn params1_is_integer_encoded() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, quad_primitive(solid())));
        });
        let buffer = encode(&scene);
        let params1 = buffer.instances[0].params1;
        assert_eq!(params1[0], 0.0);
        assert_eq!(params1[1], 0.0);
        assert_eq!(params1[2], KIND_QUAD);
        for value in params1 {
            assert_eq!(value.fract(), 0.0, "params1 必须是整数值");
        }
    }

    #[test]
    fn quad_solid_mapping() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                quad_primitive(QuadPaint::Solid {
                    fill: Color::srgb(1.0, 0.0, 0.0, 1.0),
                    border: None,
                }),
            ));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.bounds, [2.0, 4.0, 20.0, 40.0]);
        assert_eq!(instance.corner_radius, [2.0, 4.0, 6.0, 8.0]);
        assert_eq!(instance.params0, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(instance.clip_rect, [f32::MIN, f32::MIN, f32::MAX, f32::MAX]);
        assert_eq!(instance.uv, [0.0; 4]);
        assert_eq!(instance.color1, [0.0; 4]);
    }

    #[test]
    fn corner_radius_applies_css_proportional_scaling_then_the_sdf_domain_clamp() {
        let normalize = |radii: [f32; 4], width: f32, height: f32| {
            crate::math::normalize_physical_corner_radius(
                radii.map(PhysicalPx::new),
                PhysicalPx::new(width),
                PhysicalPx::new(height),
            )
            .map(f32::from)
        };
        assert_eq!(normalize([4.0; 4], 100.0, 2.0), [1.0; 4]);
        assert_eq!(normalize([8.0; 4], 4.0, 8.0), [2.0; 4]);
        assert_eq!(
            normalize([20.0, 20.0, 0.0, 0.0], 30.0, 100.0),
            [15.0, 15.0, 0.0, 0.0]
        );
        assert_eq!(
            normalize([10.0, 30.0, 0.0, 0.0], 20.0, 100.0),
            [5.0, 10.0, 0.0, 0.0]
        );
        let scaled = normalize([1.0, 0.0, 0.0, 40.0], 100.0, 20.0);
        assert!((scaled[0] - 20.0 / 41.0).abs() < 1e-5, "{scaled:?}");
        assert_eq!(scaled[3], 10.0);
        assert_eq!((scaled[1], scaled[2]), (0.0, 0.0));
        assert_eq!(normalize([-3.0; 4], 10.0, 10.0), [0.0; 4]);
        assert_eq!(normalize([f32::NAN; 4], 10.0, 10.0), [0.0; 4]);
        assert_eq!(normalize([2.0; 4], 0.0, 10.0), [0.0; 4]);
        assert_eq!(normalize([2.0; 4], 10.0, f32::NAN), [0.0; 4]);
        assert_eq!(normalize([2.0; 4], f32::INFINITY, 10.0), [0.0; 4]);

        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Quad(Quad {
                bounds: rect(0.0, 0.0, 10.0, 2.0),
                corner_radius: [px(3.0); 4],
                paint: solid(),
            }),
        ));
        scene.set_order(&[lane]);
        let buffer = encode(&scene);
        assert_eq!(buffer.instances[0].corner_radius, [2.0; 4]);

        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 2.0, 2.0),
                    corner_radius: [px(8.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        assert_eq!(
            buffer.clips[1].corner_radius, [2.0; 4],
            "clip 半径必须走同一规范化（CSS 比例缩放 + SDF 合法域）"
        );
    }

    #[test]
    fn clip_extent_reads_xywh_not_aabb() {
        let mut canvas = crate::paint::canvas::Canvas::new(rect(0.0, 0.0, 2000.0, 2000.0));
        assert!(canvas.push_clip(ClipRect {
            rect: rect(100.0, 50.0, 800.0, 600.0),
            corner_radius: [px(0.0); 4],
        }));
        canvas.push_quad(Quad {
            bounds: rect(0.0, 0.0, 2000.0, 2000.0),
            corner_radius: [px(0.0); 4],
            paint: solid(),
        });
        assert!(canvas.pop_clip());
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, Primitive::Canvas(Arc::new(canvas))));
        });
        let buffer = encode(&scene);
        let primitive = buffer
            .instances
            .iter()
            .find(|primitive| primitive.clip_rect[0] > 0.0)
            .expect("被裁剪的 quad");
        let extent = super::clip_extent(primitive).expect("有限 clip");
        assert_eq!(extent[2], 1801, "右边界必须是 x+w（不是 w）：{extent:?}");
        assert_eq!(extent[3], 1301, "下边界必须是 y+h（不是 h）：{extent:?}");
        let bounds = super::bounds_of_aabb([100.0, 50.0, 900.0, 650.0]);
        assert_eq!(
            (bounds.x0, bounds.y0, bounds.x1, bounds.y1),
            (dp(99), dp(49), dp(901), dp(651))
        );
        let empty = crate::math::DeviceRect::default();
        assert_eq!(super::bounds_of_aabb([f32::NAN, 0.0, 1.0, 1.0]), empty);
        assert_eq!(super::bounds_of_aabb([10.0, 10.0, 5.0, 5.0]), empty);
        let dot = super::bounds_of_aabb([7.0, 7.0, 7.0, 7.0]);
        assert_eq!(
            (dot.x0, dot.y0, dot.x1, dot.y1),
            (dp(6), dp(6), dp(8), dp(8))
        );
        let all = super::bounds_of_aabb([f32::MIN, f32::MIN, f32::MAX, f32::MAX]);
        assert_eq!(
            (all.x0, all.y0, all.x1, all.y1),
            (dp(i32::MIN), dp(i32::MIN), dp(i32::MAX), dp(i32::MAX))
        );
    }

    #[test]
    fn negative_origin_clip_encodes_xywh() {
        let mut canvas = crate::paint::canvas::Canvas::new(rect(0.0, 0.0, 100.0, 100.0));
        assert!(canvas.push_clip(ClipRect {
            rect: rect(-10.0, -5.0, 20.0, 20.0),
            corner_radius: [px(0.0); 4],
        }));
        canvas.push_quad(Quad {
            bounds: rect(0.0, 0.0, 30.0, 30.0),
            corner_radius: [px(0.0); 4],
            paint: solid(),
        });
        assert!(canvas.pop_clip());
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, Primitive::Canvas(Arc::new(canvas))));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[0], 1.0, "clip_index 必须非 0");
        assert_eq!(instance.clip_rect, [-20.0, -10.0, 40.0, 40.0]);
    }

    #[test]
    fn nested_rect_clips_merge_into_one_chain_entry() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [px(0.0); 4],
                },
            );
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [px(0.0); 4],
                },
            );
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(5.0, 5.0, 10.0, 10.0),
                    corner_radius: [px(0.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            for _ in 0..3 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 1.0, "链长必须是 1（不是 3）");
        let head = instance.params1[0] as usize;
        assert!(head != 0, "必须有 clip 索引");
        assert_eq!(buffer.clips[head].parent, None, "合并后的矩形项是链根");
        assert_eq!(
            buffer.clips[head].kind,
            ClipKind::Rect,
            "轴对齐矩形 clip = 像素精确 scissor"
        );
        assert_eq!(buffer.clips[head].corner_radius, [0.0; 4]);
        assert_eq!(instance.clip_rect, [10.0, 10.0, 20.0, 20.0]);
    }

    #[test]
    fn nested_identical_rounded_clips_keep_a_single_chain_entry() {
        let rounded = ClipRect {
            rect: rect(0.0, 0.0, 30.0, 30.0),
            corner_radius: [px(2.0); 4],
        };
        let scene = scene_with(|scene, lane| {
            push_clip(scene, lane, rounded);
            push_clip(scene, lane, rounded);
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 1.0, "重复层不增加链长");
        assert_eq!(buffer.clips.len(), 2, "哨兵 + 1 条圆角项");
        let head = buffer.clips[instance.params1[0] as usize];
        assert_eq!(head.kind, ClipKind::Rounded);
        assert_eq!(head.corner_radius, [4.0; 4], "2 逻辑 × scale 2");
        assert_eq!(head.parent, None);
        let coverage = clip_coverage(&buffer.clips, ClipChainId(1), (60.0, 30.0));
        assert!(
            (coverage - 0.5).abs() < 1e-4,
            "嵌套相同圆角 clip 只能乘一次覆盖率，实际 {coverage}"
        );
    }

    #[test]
    fn distinct_nested_rounded_clips_each_keep_their_entry() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [px(2.0); 4],
                },
            );
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(2.0, 2.0, 26.0, 26.0),
                    corner_radius: [px(2.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 2.0, "两层都要参与 AA");
        assert_eq!(buffer.clips.len(), 3, "哨兵 + 2 条圆角项");
        let head = buffer.clips[instance.params1[0] as usize];
        assert_eq!(head.kind, ClipKind::Rounded);
        let root = buffer.clips[head.parent.expect("第二层必须有 parent").index()];
        assert_eq!(root.kind, ClipKind::Rounded);
        assert_eq!(root.parent, None);
    }

    #[test]
    fn rect_and_rounded_clips_agree_at_their_shared_boundary() {
        let pure_rect = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 29.75, 29.75),
                    corner_radius: [px(0.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let with_rounded = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 29.75, 29.75),
                    corner_radius: [px(0.0); 4],
                },
            );
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 29.75, 29.75),
                    corner_radius: [px(2.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let rect_buffer = encode(&pure_rect);
        let rounded_buffer = encode(&with_rounded);
        let rect_head = ClipChainId(rect_buffer.instances[0].params1[0] as u32);
        let rounded_instance = rounded_buffer.instances[0];
        let rounded_head = ClipChainId(rounded_instance.params1[0] as u32);
        assert_eq!(
            rounded_instance.params1[3], 2.0,
            "矩形 + 圆角两层都要参与 AA"
        );
        let rect_root = rect_buffer.clips[rect_head.index()];
        let rounded_root = rounded_buffer.clips[rounded_head.index()];
        assert_eq!(rounded_root.kind, ClipKind::Rounded);
        let rounded_scissor = rounded_root.parent.expect("圆角项挂在矩形项下面");
        assert_eq!(
            rounded_buffer.clips[rounded_scissor.index()].kind,
            ClipKind::Rect
        );
        assert_eq!(
            rect_root.rect,
            rounded_buffer.clips[rounded_scissor.index()].rect
        );

        for pixel in [(59.5f32, 30.5f32), (30.5, 59.5), (0.5, 30.5), (30.5, 0.5)] {
            let straight = clip_coverage(&rect_buffer.clips, rect_head, pixel);
            let rounded = clip_coverage(&rounded_buffer.clips, rounded_head, pixel);
            assert_eq!(
                straight, rounded,
                "直线边上的像素 {pixel:?} 不得因叠加圆角 clip 而二次扣减 AA：{straight} vs {rounded}"
            );
            assert!(straight > 0.0, "{pixel:?} 在 clip 内");
        }
        let corner_straight = clip_coverage(&rect_buffer.clips, rect_head, (59.5, 59.5));
        let corner_rounded = clip_coverage(&rounded_buffer.clips, rounded_head, (59.5, 59.5));
        assert!(
            corner_rounded < corner_straight,
            "圆角必须真的削掉角：{corner_rounded} vs {corner_straight}"
        );
        for pixel in [(60.5f32, 30.5f32), (30.5, 60.5)] {
            assert_eq!(
                clip_coverage(&rounded_buffer.clips, rounded_head, pixel),
                0.0,
                "{pixel:?} 在 clip 外"
            );
        }
    }

    #[test]
    fn pure_rect_clip_coverage_is_binary() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.3, 30.7),
                    corner_radius: [px(0.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        let head = ClipChainId(instance.params1[0] as u32);
        assert_eq!(buffer.clips[head.index()].rect, [0.0, 0.0, 61.0, 62.0]);
        for pixel in [
            (0.5, 0.5),
            (60.5, 30.5),
            (30.5, 61.5),
            (60.5, 61.5),
            (30.5, 30.5),
        ] {
            assert_eq!(
                clip_coverage(&buffer.clips, head, pixel),
                1.0,
                "{pixel:?} 必须全保留"
            );
        }
        for pixel in [(61.5, 30.5), (30.5, 62.5), (-0.5, 30.5), (30.5, -0.5)] {
            assert_eq!(
                clip_coverage(&buffer.clips, head, pixel),
                0.0,
                "{pixel:?} 必须全裁"
            );
        }
    }

    #[test]
    fn rect_clips_cover_the_logical_rect_with_integer_pixels() {
        let cases = [
            (rect(5.15, 2.35, 10.2, 4.8), [10.0, 4.0, 21.0, 11.0]),
            (rect(-1.625, -0.375, 2.75, 1.125), [-4.0, -1.0, 7.0, 3.0]),
            (rect(4.0, 4.0, 70.0, 45.0), [8.0, 8.0, 140.0, 90.0]),
        ];
        for (logical, expected) in cases {
            let scene = scene_with(|scene, lane| {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: logical,
                        corner_radius: [px(0.0); 4],
                    },
                );
                assert!(scene.push(lane, quad_primitive(solid())));
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            });
            let buffer = encode(&scene);
            let head = ClipChainId(buffer.instances[0].params1[0] as u32);
            let clip = buffer.clips[head.index()].rect;
            assert_eq!(
                clip, expected,
                "clip {logical:?} 必须 floor/ceil 到整数物理像素"
            );
        }
    }

    #[test]
    fn mixed_rect_and_rounded_clip_chains_stay_on_integer_physical_pixels() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(1.3, 2.7, 30.35, 20.15),
                    corner_radius: [px(0.0); 4],
                },
            );
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(3.45, 4.55, 17.85, 9.25),
                    corner_radius: [px(2.5); 4],
                },
            );
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(5.7, 6.1, 40.05, 30.95),
                    corner_radius: [px(0.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        for factor in [1.0f32, 1.25, 1.5, 1.75, 2.0, 3.0] {
            let buffer = encode_at(&scene, factor);
            let instance = buffer.instances[0];
            let mut cursor = Some(ClipChainId(instance.params1[0] as u32));
            let mut kinds = Vec::new();
            while let Some(id) = cursor {
                let entry = buffer.clips[id.index()];
                for value in entry.rect {
                    assert_eq!(
                        value.fract(),
                        0.0,
                        "factor {factor}：clip 链 {kinds:?} 的物理边界必须全是整数：{:?}",
                        entry.rect
                    );
                }
                kinds.push(entry.kind);
                cursor = entry.parent;
            }
            assert_eq!(
                kinds,
                [ClipKind::Rounded, ClipKind::Rect],
                "factor {factor}：圆角项 + 合并后的矩形项"
            );
        }
    }

    #[test]
    fn zero_extent_clips_at_fractional_origins_clip_everything() {
        let zero_sized = [
            rect(10.3, 4.7, 0.0, 10.0),
            rect(10.3, 4.7, 10.0, 0.0),
            rect(10.3, 4.7, 0.0, 0.0),
            rect(-3.25, 7.9, 0.0, 20.0),
        ];
        for factor in [1.0f32, 1.25, 1.5, 1.75, 2.0, 3.0] {
            for logical in zero_sized {
                let scene = scene_with(|scene, lane| {
                    push_clip(
                        scene,
                        lane,
                        ClipRect {
                            rect: logical,
                            corner_radius: [px(0.0); 4],
                        },
                    );
                    assert!(scene.push(lane, quad_primitive(solid())));
                    assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
                });
                let buffer = encode_at(&scene, factor);
                let instance = buffer.instances[0];
                let head = ClipChainId(instance.params1[0] as u32);
                let rect = buffer.clips[head.index()].rect;
                assert_eq!(
                    (rect[2], rect[3]),
                    (0.0, 0.0),
                    "{logical:?} at {factor}：零尺寸 clip 必须写成 w = h = 0（全裁），实际 {rect:?}"
                );
                assert_eq!(
                    instance.clip_rect[2] * instance.clip_rect[3],
                    0.0,
                    "{logical:?} at {factor}：clip_rect 不得残留可绘制面积：{:?}",
                    instance.clip_rect
                );
                for pixel in [(10.5f32, 5.5f32), (10.5, 20.5), (0.5, 0.5)] {
                    assert_eq!(
                        clip_coverage(&buffer.clips, head, pixel),
                        0.0,
                        "{logical:?} at {factor}：{pixel:?} 必须被全裁（不得漏 1 px）"
                    );
                }
            }
        }
    }

    #[test]
    fn a_zero_size_rounded_clip_clips_everything() {
        for factor in [1.25f32, 1.5] {
            let logical = rect(10.3, 4.7, 0.0, 10.0);
            let scene = scene_with(|scene, lane| {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: logical,
                        corner_radius: [px(2.0); 4],
                    },
                );
                assert!(scene.push(lane, quad_primitive(solid())));
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            });
            let buffer = encode_at(&scene, factor);
            let instance = buffer.instances[0];
            let head = ClipChainId(instance.params1[0] as u32);
            let rect = buffer.clips[head.index()].rect;
            assert_eq!(
                (rect[2], rect[3]),
                (0.0, 0.0),
                "factor {factor}：零尺寸圆角 clip 也必须全裁：{rect:?}"
            );
            assert_eq!(
                buffer.clips[head.index()].corner_radius,
                [0.0; 4],
                "归零尺寸后半径随之归零"
            );
            for pixel in [(10.5f32, 5.5f32), (11.5, 5.5), (10.5, 20.5)] {
                assert_eq!(
                    clip_coverage(&buffer.clips, head, pixel),
                    0.0,
                    "factor {factor}：{pixel:?} 必须被全裁"
                );
            }
        }
    }

    #[test]
    fn gpu_clip_entries_encode_the_kind_in_meta_y() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [px(0.0); 4],
                },
            );
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(2.0, 2.0, 26.0, 26.0),
                    corner_radius: [px(3.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        let head_id = ClipChainId(instance.params1[0] as u32);
        let head = buffer.clips[head_id.index()];
        assert_eq!(head.kind, ClipKind::Rounded);
        let root_id = head.parent.expect("圆角项挂在矩形项下面");
        assert_eq!(buffer.clips[root_id.index()].kind, ClipKind::Rect);
        let mut uploaded = Vec::new();
        buffer.write_clips_gpu(&mut uploaded);
        assert_eq!(uploaded.len(), 3, "哨兵 + 矩形 + 圆角");
        assert_eq!(
            uploaded[0].meta,
            [0, CLIP_KIND_RECT as u32, 0, 0],
            "哨兵项 meta"
        );
        assert_eq!(
            uploaded[root_id.index()].meta,
            [0, CLIP_KIND_RECT as u32, 0, 0],
            "矩形项（链根）的 meta：parent = 0、kind = RECT"
        );
        assert_eq!(
            uploaded[head_id.index()].meta,
            [root_id.index() as u32, CLIP_KIND_ROUNDED as u32, 0, 0],
            "圆角项的 meta：parent = 矩形项、kind = ROUNDED"
        );
    }

    #[test]
    fn zero_sized_clip_encodes_as_a_zero_extent() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(4.0, 4.0, 0.0, 10.0),
                    corner_radius: [px(0.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert!(instance.params1[0] != 0.0, "必须有 clip 索引");
        assert!(
            instance.clip_rect[2] <= 0.0 || instance.clip_rect[3] <= 0.0,
            "零尺寸交集的 w/h 必须非正（shader 据此返回覆盖率 0）：{:?}",
            instance.clip_rect
        );
        let entry = buffer.clips[instance.params1[0] as usize];
        assert_eq!(entry.kind, ClipKind::Rect);
        assert_eq!(
            entry.rect,
            [8.0, 8.0, 0.0, 0.0],
            "矩形项必须归宿成整数的零尺寸（scissor 全裁），不得被 roundOut 变成 1 px"
        );
    }

    #[test]
    fn clip_chain_beyond_the_limit_fails_instead_of_dropping_the_outermost() {
        let scene = scene_with(|scene, lane| {
            for index in 0..12 {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: rect(index as f32, index as f32, 100.0, 100.0),
                        corner_radius: [px(0.0); 4],
                    },
                );
            }
            assert!(scene.push(lane, quad_primitive(solid())));
            for _ in 0..12 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances[0].params1[3], 1.0, "12 层矩形只占 1 项");

        let scene = scene_with(|scene, lane| {
            for index in 0..9 {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: rect(index as f32, 0.0, 100.0, 100.0),
                        corner_radius: [px(2.0); 4],
                    },
                );
            }
            assert!(scene.push(lane, quad_primitive(solid())));
            for _ in 0..9 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let error = match try_encode(&scene) {
            Err(error) => error,
            Ok(_) => panic!("9 层圆角 clip 必须报错"),
        };
        match error {
            crate::error::EngineError::ClipDepthExceeded { depth, limit } => {
                assert_eq!((depth, limit), (9, crate::paint::scene::MAX_CLIP_DEPTH));
            }
            other => panic!("期望 ClipDepthExceeded，实际 {other:?}"),
        }

        let scene = scene_with(|scene, lane| {
            for index in 0..8 {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: rect(index as f32, 0.0, 100.0, 100.0),
                        corner_radius: [px(2.0); 4],
                    },
                );
            }
            assert!(scene.push(lane, quad_primitive(solid())));
            for _ in 0..8 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances[0].params1[3], 8.0);

        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(-10.0, -10.0, 200.0, 200.0),
                    corner_radius: [px(0.0); 4],
                },
            );
            for index in 0..8 {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: rect(index as f32, 0.0, 100.0, 100.0),
                        corner_radius: [px(2.0); 4],
                    },
                );
            }
            assert!(scene.push(lane, quad_primitive(solid())));
            for _ in 0..9 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 9.0, "8 层圆角 + 1 条矩形 = 链长 9");
        assert!(
            instance.params1[3] <= MAX_CLIP_ITEMS,
            "链长必须落在 shader 的循环上界内"
        );

        assert_eq!(MAX_CLIP_ITEMS, f32::from(MAX_CLIP_DEPTH) + 1.0);

        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(-10.0, -10.0, 200.0, 200.0),
                    corner_radius: [px(0.0); 4],
                },
            );
            for index in 0..9 {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: rect(index as f32, 0.0, 100.0, 100.0),
                        corner_radius: [px(2.0); 4],
                    },
                );
            }
            assert!(scene.push(lane, quad_primitive(solid())));
        });
        match try_encode(&scene) {
            Err(crate::error::EngineError::ClipDepthExceeded { depth, limit }) => {
                assert_eq!((depth, limit), (9, MAX_CLIP_DEPTH), "报的是圆角层数");
            }
            Err(other) => panic!("9 层圆角 + 矩形必须报 ClipDepthExceeded，实际 {other:?}"),
            Ok(_) => panic!("9 层圆角 + 矩形必须报 ClipDepthExceeded"),
        }
    }

    #[test]
    fn canvas_rounded_clips_under_an_ancestor_rect_fit_the_chain_limit() {
        let mut canvas = Canvas::new(rect(0.0, 0.0, 64.0, 32.0));
        for index in 0..usize::from(MAX_CLIP_DEPTH) {
            assert!(
                canvas.push_clip(ClipRect {
                    rect: rect(index as f32, 0.0, 30.0, 30.0),
                    corner_radius: [px(2.0); 4],
                }),
                "Canvas 门限只数圆角层：第 8 层圆角必须允许"
            );
        }
        canvas.push_quad(Quad {
            bounds: rect(1.0, 2.0, 10.0, 20.0),
            corner_radius: [px(0.0); 4],
            paint: solid(),
        });
        for _ in 0..usize::from(MAX_CLIP_DEPTH) {
            assert!(canvas.pop_clip());
        }

        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(-10.0, -10.0, 200.0, 200.0),
                    corner_radius: [px(0.0); 4],
                },
            );
            assert!(scene.push(lane, Primitive::Canvas(Arc::new(canvas))));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 9.0, "8 层圆角 + 1 条矩形 = 链长 9");
        assert_eq!(instance.params1[3], MAX_CLIP_ITEMS);
    }

    #[test]
    fn params1_w_carries_the_clip_chain_depth() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, quad_primitive(solid())));
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [px(1.0); 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances[0].params1[3], 0.0, "无裁剪 ⇒ 链长 0");
        assert_eq!(buffer.instances[1].params1[3], 1.0);
        assert_eq!(
            buffer.instances[0].params1[0], 0.0,
            "clip_index == 0 = 无裁剪"
        );
    }

    #[test]
    fn quad_border_uses_color1() {
        let border = Border {
            width: px(2.0),
            color: Color::BLACK,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                quad_primitive(QuadPaint::Solid {
                    fill: Color::WHITE,
                    border: Some(border),
                }),
            ));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params0[0], 4.0);
        assert_eq!(instance.color1, resolver().resolve(Color::BLACK).to_array());
        assert_eq!(instance.params0[3], 1.0);
    }

    #[test]
    fn border_width_snaps_to_integer_device_pixels() {
        for (factor, expected) in [
            (1.0, 1.0),
            (1.25, 1.0),
            (1.5, 2.0),
            (1.75, 2.0),
            (2.0, 2.0),
            (3.0, 3.0),
        ] {
            for offset in [0.0f32, 0.1, 0.25, 0.4, 0.49, 0.5, 0.75, 0.9] {
                let scene = scene_with(|scene, lane| {
                    assert!(scene.push(
                        lane,
                        Primitive::Quad(Quad {
                            bounds: rect(offset, offset, 20.0, 10.0),
                            corner_radius: [px(0.0); 4],
                            paint: QuadPaint::Solid {
                                fill: Color::WHITE,
                                border: Some(Border {
                                    width: px(1.0),
                                    color: Color::BLACK,
                                }),
                            },
                        }),
                    ));
                });
                let buffer = encode_at(&scene, factor);
                let instance = buffer.instances[0];
                assert_eq!(
                    instance.params0[0], expected,
                    "1 dp 边框在 {factor}× / offset {offset} 下必须是 {expected} 物理像素"
                );
                assert_eq!(
                    instance.params0[0].fract(),
                    0.0,
                    "边框带宽必须是整数物理像素"
                );
            }
        }
    }

    #[test]
    fn hairline_border_never_disappears() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: rect(10.0, 10.0, 20.0, 10.0),
                    corner_radius: [px(0.0); 4],
                    paint: QuadPaint::Solid {
                        fill: Color::TRANSPARENT,
                        border: Some(Border {
                            width: px(0.25),
                            color: Color::BLACK,
                        }),
                    },
                }),
            ));
        });
        let buffer = encode_at(&scene, 1.25);
        assert_eq!(
            buffer.instances[0].params0[0], 1.0,
            "0.25 dp 边框在 125% 下也必须画出 1 物理像素"
        );
    }

    #[test]
    fn quads_snap_to_device_pixels() {
        for factor in [1.0f32, 1.25, 1.5, 1.75, 2.0, 3.0] {
            for (x, y, width, height) in [
                (10.25f32, 4.7f32, 20.4f32, 9.6f32),
                (0.0, 0.0, 1.0, 1.0),
                (-3.3, 7.9, 16.75, 2.5),
                (0.3, 0.3, 0.4, 0.4),
            ] {
                let logical = rect(x, y, width, height);
                let scene = scene_with(|scene, lane| {
                    assert!(scene.push(
                        lane,
                        Primitive::Quad(Quad {
                            bounds: logical,
                            corner_radius: [px(0.0); 4],
                            paint: QuadPaint::Solid {
                                fill: Color::WHITE,
                                border: None,
                            },
                        }),
                    ));
                });
                let buffer = encode_at(&scene, factor);
                let Some(instance) = buffer.instances.first() else {
                    assert!(
                        width * factor < 1.0 || height * factor < 1.0,
                        "{logical:?} at {factor}：跨度 ≥ 1 物理像素的 Quad 不得消失"
                    );
                    continue;
                };
                let [px, py, pw, ph] = instance.bounds;
                assert_eq!(px, (x * factor).round(), "左边界 round");
                assert_eq!(py, (y * factor).round(), "上边界 round");
                assert_eq!(
                    px + pw,
                    ((x + width) * factor).round(),
                    "右边界必须由完整右坐标 round 后相减"
                );
                assert_eq!(
                    py + ph,
                    ((y + height) * factor).round(),
                    "下边界必须由完整下坐标 round 后相减"
                );
                let exact = width * factor;
                assert!(
                    (pw - exact).abs() <= 1.0 + f32::EPSILON,
                    "{logical:?} at {factor}：吸附宽度与未吸附值最多差 1 物理像素（{pw} vs {exact}）"
                );
            }
        }
    }

    #[test]
    fn image_and_quad_share_physical_edges() {
        let tile = AtlasTile {
            atlas: AtlasKind::Image,
            page: 0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
        };
        for factor in [1.0f32, 1.25, 1.5, 1.75, 2.0, 3.0] {
            for (x, y, width, height) in [
                (10.25f32, 4.7f32, 20.4f32, 9.6f32),
                (0.1, 0.9, 3.3, 7.7),
                (-2.6, 5.15, 12.8, 6.45),
            ] {
                let logical = rect(x, y, width, height);
                let quad = scene_with(|scene, lane| {
                    assert!(scene.push(
                        lane,
                        Primitive::Quad(Quad {
                            bounds: logical,
                            corner_radius: [px(0.0); 4],
                            paint: QuadPaint::Solid {
                                fill: Color::WHITE,
                                border: None,
                            },
                        }),
                    ));
                });
                let image = scene_with(|scene, lane| {
                    assert!(scene.push(
                        lane,
                        Primitive::Image(Image {
                            bounds: logical,
                            tile,
                            modulation: Color::WHITE,
                        }),
                    ));
                });
                let quad_bounds = encode_at(&quad, factor).instances[0].bounds;
                let image_bounds = encode_at(&image, factor).instances[0].bounds;
                assert_eq!(
                    quad_bounds, image_bounds,
                    "{logical:?} at {factor}：Quad 与 Image 必须落在同一物理边界上"
                );
            }
        }
    }

    #[test]
    fn borders_are_integer_device_pixels() {
        for factor in [1.0f32, 1.25, 1.5, 2.0, 3.0] {
            for offset in [0.0f32, 0.1, 0.25, 0.4, 0.49, 0.5, 0.75, 0.9] {
                let scene = scene_with(|scene, lane| {
                    assert!(scene.push(
                        lane,
                        Primitive::Quad(Quad {
                            bounds: rect(offset, offset, 20.0, 10.0),
                            corner_radius: [px(0.0); 4],
                            paint: QuadPaint::Solid {
                                fill: Color::TRANSPARENT,
                                border: Some(Border {
                                    width: px(1.0),
                                    color: Color::BLACK,
                                }),
                            },
                        }),
                    ));
                });
                let instance = encode_at(&scene, factor).instances[0];
                let [px, py, pw, ph] = instance.bounds;
                let width = instance.params0[0];
                assert_eq!(
                    width.fract(),
                    0.0,
                    "1 dp 边框在 {factor}× / offset {offset} 下必须是整数物理像素"
                );
                assert!(width >= 1.0, "边框必须至少 1 物理像素：{width}");
                assert!(
                    width <= pw.min(ph),
                    "边框不得超出短边：width {width}, quad {pw}×{ph}"
                );
                for inner in [px + pw - width, py + ph - width] {
                    assert_eq!(inner.fract(), 0.0, "右/下内边界必须是整数物理像素：{inner}");
                }
                assert!(
                    px + pw - width >= px && py + ph - width >= py,
                    "右/下内边界不得越过左/上外边界：outer ({px},{py}) size ({pw},{ph}) width {width}"
                );
            }
        }
    }

    #[test]
    fn a_one_physical_pixel_short_side_quad_is_all_border() {
        for factor in [1.0f32, 2.0, 3.0] {
            for offset in [0.0f32, 0.3, 0.7] {
                let logical_height = 1.0 / factor;
                let scene = scene_with(|scene, lane| {
                    assert!(scene.push(
                        lane,
                        Primitive::Quad(Quad {
                            bounds: rect(offset + 10.0, offset + 10.0, 20.0, logical_height),
                            corner_radius: [px(0.0); 4],
                            paint: QuadPaint::Solid {
                                fill: Color::TRANSPARENT,
                                border: Some(Border {
                                    width: px(1.0),
                                    color: Color::BLACK,
                                }),
                            },
                        }),
                    ));
                });
                let instance = encode_at(&scene, factor).instances[0];
                let [_, _, pw, ph] = instance.bounds;
                assert_eq!(ph, 1.0, "短边必须恰好 1 物理像素（factor {factor}）");
                assert_eq!(
                    instance.params0[0], 1.0,
                    "短边 1 物理像素的 Quad 整块都是边框（不得被 clamp 到半个像素）：{pw}×{ph}"
                );
                assert_eq!(
                    instance.params0[0].fract(),
                    0.0,
                    "边框宽度必须是整数物理像素"
                );
            }
        }
    }

    #[test]
    fn stroke_width_is_phase_independent_at_fractional_scales() {
        for factor in [1.25f32, 1.5, 1.75] {
            let mut observed_width = None;
            for offset in [0.0f32, 0.1, 0.25, 0.4, 0.49, 0.5, 0.6, 0.75, 0.9] {
                let scene = scene_with(|scene, lane| {
                    assert!(scene.push(
                        lane,
                        Primitive::Quad(Quad {
                            bounds: rect(offset, offset, 20.0, 10.0),
                            corner_radius: [px(0.0); 4],
                            paint: QuadPaint::Solid {
                                fill: Color::TRANSPARENT,
                                border: Some(Border {
                                    width: px(1.0),
                                    color: Color::BLACK,
                                }),
                            },
                        }),
                    ));
                });
                let instance = encode_at(&scene, factor).instances[0];
                let [px, py, pw, ph] = instance.bounds;
                let width = instance.params0[0];
                let exact_outer = offset * factor;
                let exact_inner = (offset + 1.0) * factor;
                assert_eq!(
                    px,
                    exact_outer.round(),
                    "外边界必须吸到最近的整数物理像素：offset {offset} at {factor}"
                );
                assert_eq!(
                    (px + width).fract(),
                    0.0,
                    "内边界必须是整数物理像素：offset {offset} at {factor}"
                );
                assert!(
                    ((px + width) - exact_inner).abs() <= 1.0 + f32::EPSILON,
                    "内边界与精确位置最多差 1 物理像素：offset {offset} at {factor}"
                );
                assert_eq!(
                    pw,
                    ((offset + 20.0) * factor).round() - px,
                    "右边界由完整右坐标 round 后相减"
                );
                assert_eq!(
                    ph,
                    ((offset + 10.0) * factor).round() - py,
                    "下边界由完整下坐标 round 后相减"
                );
                match observed_width {
                    None => observed_width = Some(width),
                    Some(want) => assert_eq!(
                        width, want,
                        "边框宽度必须与相位无关：offset {offset} at {factor}"
                    ),
                }
            }
        }
    }

    #[test]
    fn quad_gradient_kinds() {
        for (kind, expected) in [
            (GradientKind::Horizontal, GRADIENT_HORIZONTAL),
            (GradientKind::Vertical, GRADIENT_VERTICAL),
            (GradientKind::Radial, GRADIENT_RADIAL),
        ] {
            let scene = scene_with(|scene, lane| {
                assert!(scene.push(
                    lane,
                    quad_primitive(QuadPaint::Gradient(Gradient {
                        from: Color::WHITE,
                        to: Color::BLACK,
                        kind,
                    })),
                ));
            });
            let buffer = encode(&scene);
            let instance = buffer.instances[0];
            assert_eq!(instance.params0[2], expected);
            assert_eq!(instance.params0[0], 0.0);
            assert_eq!(instance.color1, resolver().resolve(Color::BLACK).to_array());
        }
    }

    #[test]
    fn glyph_run_kind_follows_the_tile_atlas() {
        let tile = |atlas: AtlasKind| AtlasTile {
            atlas,
            page: 0,
            u0: 0.0,
            v0: 0.0,
            u1: 0.5,
            v1: 0.5,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
        };
        let scene = scene_with(|scene, lane| {
            for atlas in [AtlasKind::Glyph, AtlasKind::ColorGlyph] {
                assert!(scene.push(
                    lane,
                    Primitive::GlyphRun(GlyphRun {
                        color: Color::WHITE,
                        glyphs: vec![PositionedGlyph {
                            tile: tile(atlas),
                            position: LogicalPoint::new(px(1.0), px(2.0)),
                            size: LogicalSize::new(px(8.0), px(8.0)),
                        }],
                    }),
                ));
            }
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances.len(), 2);
        assert_eq!(buffer.instances[0].params1[2], KIND_GLYPH);
        assert_eq!(buffer.instances[1].params1[2], KIND_COLOR_GLYPH);
        assert_eq!(buffer.instances[1].params1[1], 0.0, "彩色字形图集恒第 0 层");
        assert_eq!(buffer.instances[0].params1[1], 0.0);
    }

    #[test]
    fn atlas_bounds_are_integer() {
        let tile = AtlasTile {
            atlas: AtlasKind::Glyph,
            page: 0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::GlyphRun(GlyphRun {
                    color: Color::WHITE,
                    glyphs: vec![PositionedGlyph {
                        tile,
                        position: LogicalPoint::new(px(1.0), px(2.0)),
                        size: LogicalSize::new(px(3.0), px(4.0)),
                    }],
                }),
            ));
        });
        let buffer = encode(&scene);
        for value in buffer.instances[0].bounds {
            assert_eq!(value.fract(), 0.0);
        }
    }

    #[test]
    fn glyph_run_is_one_to_n() {
        let tile = |u0: f32| AtlasTile {
            atlas: AtlasKind::Glyph,
            page: 0,
            u0,
            v0: 0.0,
            u1: u0 + 0.1,
            v1: 0.1,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::GlyphRun(GlyphRun {
                    color: Color::WHITE,
                    glyphs: vec![
                        PositionedGlyph {
                            tile: tile(0.0),
                            position: LogicalPoint::new(px(0.0), px(0.0)),
                            size: LogicalSize::new(px(2.0), px(2.0)),
                        },
                        PositionedGlyph {
                            tile: tile(0.1),
                            position: LogicalPoint::new(px(2.0), px(0.0)),
                            size: LogicalSize::new(px(2.0), px(2.0)),
                        },
                        PositionedGlyph {
                            tile: tile(0.2),
                            position: LogicalPoint::new(px(4.0), px(0.0)),
                            size: LogicalSize::new(px(2.0), px(2.0)),
                        },
                    ],
                }),
            ));
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances.len(), 3);
        assert!(
            buffer.instances.iter().all(|i| i.params1[1] == 0.0),
            "字形图集恒 1 层：层号必须是 0"
        );
        assert!(buffer.instances.iter().all(|i| i.params1[2] == KIND_GLYPH));
        assert_ne!(buffer.instances[0].uv, buffer.instances[1].uv);
        assert_ne!(buffer.instances[0].bounds, buffer.instances[1].bounds);
    }

    #[test]
    fn shadow_sigma_and_padding_follow_css() {
        assert_eq!(SHADOW_SIGMA_SCALE, 0.5, "CSS box-shadow：σ = blur / 2");
        assert_eq!(SHADOW_SIGMA_MIN, 0.35);
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Shadow(Shadow {
                    rect: rect(0.0, 0.0, 10.0, 10.0),
                    offset: LogicalPoint::new(px(2.0), px(3.0)),
                    blur: px(4.0),
                    color: Color::BLACK,
                    corner_radius: [px(2.0); 4],
                }),
            ));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.bounds, [4.0, 6.0, 20.0, 20.0]);
        assert_eq!(instance.params0[1], 8.0, "物理 blur = 4 × scale 2");
        assert_eq!(instance.params0[3], 13.0);
        assert_eq!(instance.params1[2], KIND_SHADOW);
        assert_eq!(instance.corner_radius, [4.0; 4]);
    }

    #[test]
    fn shadow_corner_radius_is_normalized_like_a_quad() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Shadow(Shadow {
                    rect: rect(0.0, 0.0, 4.0, 8.0),
                    offset: LogicalPoint::ZERO,
                    blur: px(0.0),
                    color: Color::BLACK,
                    corner_radius: [px(100.0); 4],
                }),
            ));
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances[0].corner_radius, [4.0; 4]);
        assert!((buffer.instances[0].params0[3] - (3.0 * SHADOW_SIGMA_MIN + 1.0)).abs() < 1e-6);
    }

    #[test]
    fn op_bounds_cover_the_shader_vertex_padding() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Shadow(Shadow {
                    rect: rect(10.0, 10.0, 20.0, 20.0),
                    offset: LogicalPoint::ZERO,
                    blur: px(8.0),
                    color: Color::BLACK,
                    corner_radius: [px(0.0); 4],
                }),
            ));
        });
        let buffer = encode(&scene);
        let crate::paint::instance::FrameOp::Instances { bounds, .. } = buffer.ops[0] else {
            panic!("阴影应产生一个 Instances op");
        };
        assert_eq!(
            (bounds.x0, bounds.y0, bounds.x1, bounds.y1),
            (dp(-6), dp(-6), dp(86), dp(86)),
            "op 包围盒必须覆盖阴影的模糊环（3σ + 1 的顶点外扩）"
        );
    }

    #[test]
    fn skip_transparent_and_empty() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: rect(0.0, 0.0, 10.0, 10.0),
                    corner_radius: [px(0.0); 4],
                    paint: QuadPaint::Solid {
                        fill: Color::TRANSPARENT,
                        border: None,
                    },
                }),
            ));
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: LogicalRect::default(),
                    corner_radius: [px(0.0); 4],
                    paint: solid(),
                }),
            ));
            assert!(scene.push(
                lane,
                Primitive::GlyphRun(GlyphRun {
                    color: Color::TRANSPARENT,
                    glyphs: vec![],
                }),
            ));
        });
        let buffer = encode(&scene);
        assert!(buffer.instances.is_empty());
    }

    #[test]
    fn clip_chain_dedup() {
        let clip = ClipRect {
            rect: rect(0.0, 0.0, 30.0, 30.0),
            corner_radius: [px(0.0); 4],
        };
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();
        for lane in [a, b] {
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Push(clip))));
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        }
        scene.set_order(&[a, b]);
        let buffer = encode(&scene);
        assert_eq!(buffer.instances.len(), 2);
        assert_eq!(
            buffer.instances[0].params1[0],
            buffer.instances[1].params1[0]
        );
        assert_eq!(buffer.instances[0].params1[0], 1.0);
        assert_eq!(buffer.clips.len(), 2, "哨兵 + 一条去重后的链");
    }

    #[test]
    fn geometry_op_stores_clip_aabb() {
        let geometry = GeometryId::for_test(0, 1);
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Clip(ClipOp::Push(ClipRect {
                rect: rect(100.0, 50.0, 800.0, 600.0),
                corner_radius: [px(0.0); 4],
            }))
        ));
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw::full(
                geometry,
                Transform::IDENTITY,
                crate::paint::color::Color::WHITE,
            )),
        ));
        assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        scene.set_order(&[lane]);
        let buffer = encode(&scene);
        let Some(super::FrameOp::Geometry { clip_rect, .. }) = buffer
            .ops
            .iter()
            .find(|op| matches!(op, super::FrameOp::Geometry { .. }))
            .copied()
        else {
            panic!("必须有 geometry op")
        };
        let aabb = clip_rect.map(f32::from_bits);
        assert_eq!(
            aabb,
            [200.0, 100.0, 1800.0, 1300.0],
            "geometry op 必须存物理 AABB (x0, y0, x1, y1)（逻辑 × scale）"
        );
    }

    #[test]
    fn geometry_op_bounds_follow_the_transform() {
        let stride = crate::paint::geometry::GeometryPipeline::Ui.vertex_stride() as usize;
        let points = [[0.0f32, 0.0, -1.0], [10.0, 0.0, -1.0], [0.0, 10.0, -1.0]];
        let vertex_bytes: Vec<u8> = points
            .iter()
            .flat_map(|point| point.iter().flat_map(|value| value.to_ne_bytes()))
            .collect();
        let desc = crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from(vec![0u8, 0, 1, 0, 2, 0]),
            vertex_stride: stride as u32,
            index_type: crate::paint::geometry::IndexType::U16,
            topology: crate::paint::geometry::Topology::TriangleList,
            bounds: rect(0.0, 0.0, 10.0, 10.0),
            pipeline: crate::paint::geometry::GeometryPipeline::Ui,
        };
        let mut store = crate::paint::geometry_store::GeometryStore::new();
        let id = store.insert(desc).expect("valid geometry");

        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw::full(
                id,
                Transform::translate(px(5.0), px(5.0)),
                crate::paint::color::Color::WHITE,
            )),
        ));
        scene.set_order(&[lane]);

        let mut encoder = InstanceEncoder::new();
        let buffer = encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("encode");
        let Some(super::FrameOp::Geometry { bounds, .. }) = buffer
            .ops
            .iter()
            .find(|op| matches!(op, super::FrameOp::Geometry { .. }))
        else {
            panic!("必须有 geometry op")
        };
        assert_eq!(
            (bounds.x0, bounds.y0, bounds.x1, bounds.y1),
            (dp(9), dp(9), dp(31), dp(31)),
            "几何 op 的可见范围必须用 transform 后的 AABB"
        );
    }

    #[test]
    fn encoder_uses_the_supplied_geometry_store() {
        let stride = crate::paint::geometry::GeometryPipeline::Ui.vertex_stride() as usize;
        let vertex_bytes = bytes::Bytes::from(vec![0u8; stride * 3]);
        let index_bytes = bytes::Bytes::from(vec![0u8, 0, 1, 0, 2, 0]);
        let desc = crate::paint::geometry::GeometryDesc {
            vertex_bytes,
            index_bytes,
            vertex_stride: stride as u32,
            index_type: crate::paint::geometry::IndexType::U16,
            topology: crate::paint::geometry::Topology::TriangleList,
            bounds: rect(0.0, 0.0, 10.0, 10.0),
            pipeline: crate::paint::geometry::GeometryPipeline::Ui,
        };
        let mut store = crate::paint::geometry_store::GeometryStore::new();
        let id = store.insert(desc).expect("valid geometry");

        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw::full(
                id,
                Transform::IDENTITY,
                crate::paint::color::Color::WHITE,
            )),
        ));
        scene.set_order(&[lane]);
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("encode");
        let draw = buffer.geometry.first().expect("必须有一条几何绘制");
        assert_eq!(
            draw.snapshot.vertex_len() as usize,
            stride * 3,
            "编码器必须从传入的 store 取到真实顶点（而不是空 MISSING 描述）"
        );
        assert_eq!(draw.snapshot.index_len(), 6);
    }

    fn register_triangles(
        triangles: u32,
    ) -> (crate::paint::geometry_store::GeometryStore, GeometryId) {
        let mut vertices = Vec::with_capacity(triangles as usize * 36);
        let mut indices = Vec::with_capacity(triangles as usize * 6);
        for triangle in 0..triangles {
            let base = (triangle * 3) as u16;
            let x = (triangle % 64) as f32;
            let y = (triangle / 64) as f32;
            for (dx, dy) in [(0.0f32, 0.0f32), (1.0, 0.0), (0.0, 1.0)] {
                vertices.extend_from_slice(&(x + dx).to_ne_bytes());
                vertices.extend_from_slice(&(y + dy).to_ne_bytes());
                vertices.extend_from_slice(&(-1.0f32).to_ne_bytes());
            }
            for index in [base, base + 1, base + 2] {
                indices.extend_from_slice(&index.to_ne_bytes());
            }
        }
        let mut store = crate::paint::geometry_store::GeometryStore::new();
        let id = store
            .insert(crate::paint::geometry::GeometryDesc {
                vertex_bytes: bytes::Bytes::from(vertices),
                index_bytes: bytes::Bytes::from(indices),
                vertex_stride: crate::paint::geometry::GeometryPipeline::Ui.vertex_stride(),
                index_type: crate::paint::geometry::IndexType::U16,
                topology: crate::paint::geometry::Topology::TriangleList,
                bounds: LogicalRect::new(px(0.0), px(0.0), px(64.0), px(100.0)),
                pipeline: crate::paint::geometry::GeometryPipeline::Ui,
            })
            .expect("注册几何");
        (store, id)
    }

    struct GeometryEncoded {
        ops: Vec<super::FrameOp>,
        ranges: Vec<std::ops::Range<u32>>,
        encoded_chunks: usize,
    }

    fn encode_geometry(
        scene: &Scene,
        store: &crate::paint::geometry_store::GeometryStore,
    ) -> GeometryEncoded {
        let mut encoder = InstanceEncoder::new();
        let (ops, ranges) = {
            let buffer = encoder
                .encode(scene, scale(), resolver(), store)
                .expect("encode geometry scene");
            (
                buffer.ops.clone(),
                buffer
                    .geometry
                    .iter()
                    .map(|draw| draw.range.clone())
                    .collect(),
            )
        };
        GeometryEncoded {
            ops,
            ranges,
            encoded_chunks: encoder.encoded_geometry_chunks(),
        }
    }

    fn range_draw(id: GeometryId, range: std::ops::Range<u32>) -> Primitive {
        Primitive::Geometry(GeometryDraw {
            id,
            transform: Transform::IDENTITY,
            color: Color::WHITE,
            range,
        })
    }

    #[test]
    fn geometry_range_expands_to_one_op_per_intersecting_chunk() {
        let (store, id) = register_triangles(4096 + 3);

        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, range_draw(id, 0..u32::MAX)));
        });
        let encoded = encode_geometry(&scene, &store);
        assert_eq!(encoded.ops.len(), 2, "整份几何 = 两块 = 两个几何 op");
        assert_eq!(encoded.ranges, vec![0..12288, 12288..12297]);
        assert_eq!(encoded.encoded_chunks, 2);

        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, range_draw(id, 12285..12294)));
        });
        let encoded = encode_geometry(&scene, &store);
        assert_eq!(encoded.ranges, vec![12285..12288, 12288..12294]);

        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, range_draw(id, 6..12)));
        });
        let encoded = encode_geometry(&scene, &store);
        assert_eq!(encoded.ranges, vec![6..12]);
        assert_eq!(encoded.ops.len(), 1);
    }

    #[test]
    fn empty_geometry_ranges_produce_no_op() {
        let (store, id) = register_triangles(3);
        for range in [6..6, std::ops::Range { start: 9, end: 3 }, 0..0, 9..12] {
            let scene = scene_with(|scene, lane| {
                assert!(scene.push(lane, range_draw(id, range.clone())));
            });
            let encoded = encode_geometry(&scene, &store);
            assert!(
                encoded.ops.is_empty(),
                "range {range:?} 不得产生几何 op：{:?}",
                encoded.ranges
            );
            assert!(encoded.ranges.is_empty());
            assert_eq!(encoded.encoded_chunks, 0);
        }
    }

    #[test]
    fn unaligned_triangle_list_range_is_clamped_to_whole_triangles() {
        let (store, id) = register_triangles(4);
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, range_draw(id, 0..10)));
        });
        let encoded = encode_geometry(&scene, &store);
        assert_eq!(encoded.ranges, vec![0..9], "10 个索引只能画 3 个三角形");
    }

    #[test]
    fn multiple_draws_of_one_geometry_keep_their_own_ranges() {
        let (store, id) = register_triangles(4096 + 3);
        let scene = scene_with(|scene, lane| {
            for range in [0..6, 12288..12291, 6..6] {
                assert!(scene.push(lane, range_draw(id, range)));
            }
        });
        let encoded = encode_geometry(&scene, &store);
        assert_eq!(
            encoded.ranges,
            vec![0..6, 12288..12291],
            "空 range 的 draw 不产生 op，其余保持绘制序"
        );
        assert_eq!(encoded.ops.len(), 2);
    }

    #[test]
    fn path_draws_every_chunk() {
        let (store, id) = register_triangles(4096 + 3);
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Path(crate::paint::primitive::Path {
                    bounds: rect(0.0, 0.0, 10.0, 10.0),
                    geometry: id,
                    fill: Color::WHITE,
                })
            ));
        });
        let encoded = encode_geometry(&scene, &store);
        assert_eq!(encoded.ranges, vec![0..12288, 12288..12297]);
    }

    #[test]
    fn gpu_draw_indexed_validates_against_the_uploaded_index_count() {
        let (store, id) = register_triangles(3);
        let snapshot = store.snapshot_arc(id).expect("快照");
        let draw = |range: std::ops::Range<u32>| crate::paint::instance::GeometryDrawGpu {
            id,
            range,
            transform: Transform::IDENTITY,
            color: [1.0; 4],
            snapshot: Arc::clone(&snapshot),
        };
        assert_eq!(
            draw(3..9).draw_indexed(9).expect("合法区间"),
            DrawIndexedArgs {
                first_index: 3,
                count: 6,
                base_vertex: 0,
            },
            "count 是区间长度（不是 range.end）：三端参数由同一纯函数推导"
        );
        assert!(
            draw(3..10).draw_indexed(9).is_err(),
            "越过已上传的索引数必须报 GeometryRegister，不得静默越界读"
        );
        assert!(
            draw(3..3).draw_indexed(9).is_err(),
            "空区间不应出现在几何 op 表里"
        );
        assert!(
            draw(std::ops::Range { start: 9, end: 3 })
                .draw_indexed(9)
                .is_err(),
            "反向区间必须报 GeometryRegister"
        );
    }

    #[test]
    fn draw_indexed_params_passes_through_the_clipped_range() {
        assert_eq!(
            draw_indexed_params(3, 6, 9).expect("合法区间"),
            DrawIndexedArgs {
                first_index: 3,
                count: 6,
                base_vertex: 0,
            }
        );
        assert_eq!(
            draw_indexed_params(12288, 9, 12297).expect("尾块"),
            DrawIndexedArgs {
                first_index: 12288,
                count: 9,
                base_vertex: 0,
            }
        );
    }

    #[test]
    fn draw_indexed_params_rejects_empty_count() {
        match draw_indexed_params(3, 0, 9) {
            Err(EngineError::GeometryRegister { .. }) => {}
            other => panic!("count = 0 必须报 GeometryRegister，实际 {other:?}"),
        }
    }

    #[test]
    fn draw_indexed_params_rejects_out_of_bounds_ranges() {
        match draw_indexed_params(3, 7, 9) {
            Err(EngineError::GeometryRegister { .. }) => {}
            other => panic!("越过已上传索引数必须报 GeometryRegister，实际 {other:?}"),
        }
        match draw_indexed_params(9, 1, 9) {
            Err(EngineError::GeometryRegister { .. }) => {}
            other => panic!("first_index == uploaded_index_count 必须报错，实际 {other:?}"),
        }
    }

    #[test]
    fn draw_indexed_params_rejects_overflowing_ranges() {
        match draw_indexed_params(u32::MAX, 2, u32::MAX) {
            Err(EngineError::GeometryRegister { .. }) => {}
            other => panic!("first_index + count 溢出必须报错（不得回绕），实际 {other:?}"),
        }
        match draw_indexed_params(u32::MAX - 1, 3, u32::MAX) {
            Err(EngineError::GeometryRegister { .. }) => {}
            other => panic!("first_index + count 溢出必须报错（不得回绕），实际 {other:?}"),
        }
    }

    #[test]
    fn draw_indexed_params_always_uses_zero_base_vertex() {
        for (first_index, count, uploaded) in [(0, 3, 3), (0, 12288, 12297), (12288, 9, 12297)] {
            let args = draw_indexed_params(first_index, count, uploaded).expect("合法区间");
            assert_eq!(
                args.base_vertex, 0,
                "first_index={first_index} count={count}"
            );
        }
    }

    #[test]
    fn frame_ops_preserve_order() {
        let geometry = GeometryId::for_test(0, 1);
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(lane, quad_primitive(solid())));
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw::full(
                geometry,
                Transform::IDENTITY,
                crate::paint::color::Color::WHITE,
            )),
        ));
        assert!(scene.push(lane, quad_primitive(solid())));
        scene.set_order(&[lane]);
        let buffer = encode(&scene);
        assert_eq!(buffer.ops.len(), 3);
        assert!(matches!(
            buffer.ops[0],
            super::FrameOp::Instances {
                start: 0,
                len: 1,
                ..
            }
        ));
        assert!(matches!(
            buffer.ops[1],
            super::FrameOp::Geometry { index: 0, .. }
        ));
        assert!(matches!(
            buffer.ops[2],
            super::FrameOp::Instances {
                start: 1,
                len: 1,
                ..
            }
        ));
    }

    #[test]
    fn image_maps_to_atlas_quad() {
        let tile = AtlasTile {
            atlas: AtlasKind::Image,
            page: 0,
            u0: 0.1,
            v0: 0.2,
            u1: 0.3,
            v1: 0.4,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Image(crate::paint::primitive::Image {
                    bounds: rect(1.0, 2.0, 10.0, 20.0),
                    tile,
                    modulation: Color::WHITE,
                }),
            ));
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances.len(), 1);
        let instance = buffer.instances[0];
        assert_eq!(instance.bounds, [2.0, 4.0, 20.0, 40.0]);
        assert_eq!(
            instance.params1[1],
            tile.page_f32(),
            "params1.y 必须是图集页（texture array 层号），不是旧的 texture_index"
        );
        assert_eq!(instance.params1[2], super::KIND_IMAGE);
        assert_eq!(instance.uv, [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(instance.params1[3], 0.0, "无裁剪 ⇒ clip_depth 0");
    }

    #[test]
    #[should_panic(expected = "image tile page")]
    fn image_page_out_of_range_is_rejected_at_encode_time() {
        let tile = AtlasTile {
            atlas: AtlasKind::Image,
            page: crate::paint::atlas::MAX_IMAGE_PAGES,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Image(crate::paint::primitive::Image {
                    bounds: rect(0.0, 0.0, 4.0, 4.0),
                    tile,
                    modulation: Color::WHITE,
                }),
            ));
        });
        let _ = encode(&scene);
    }

    #[test]
    fn image_encodes_the_atlas_page_as_the_texture_array_layer() {
        for page in [0u16, 1, 2, 3] {
            let tile = AtlasTile {
                atlas: AtlasKind::Image,
                page,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                generation: 1,
                slot: 0,
                slot_epoch: 0,
            };
            let scene = scene_with(|scene, lane| {
                assert!(scene.push(
                    lane,
                    Primitive::Image(crate::paint::primitive::Image {
                        bounds: rect(0.0, 0.0, 1.0, 1.0),
                        tile,
                        modulation: Color::WHITE,
                    }),
                ));
            });
            let buffer = encode(&scene);
            assert_eq!(
                buffer.instances[0].params1[1],
                f32::from(page),
                "页号必须整数编码进 params1.y"
            );
            assert_eq!(buffer.instances[0].params1[1].fract(), 0.0);
        }
    }

    #[test]
    fn encode_is_idempotent() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(
                lane,
                quad_primitive(QuadPaint::Solid {
                    fill: Color::srgb(1.0, 1.0, 1.0, 0.5),
                    border: None,
                }),
            ));
        });
        let first = encode(&scene);
        let second = encode(&scene);
        assert_eq!(first.instances[0].params1, second.instances[0].params1);
        assert_eq!(first.instances[0].color0, second.instances[0].color0);
        assert_eq!(first.instances[1].color0, second.instances[1].color0);
    }

    #[test]
    fn clip_state_depth_counts_the_merged_rect_once() {
        let state = ClipState::unbounded();
        assert_eq!(state.depth(), 0);
        assert!(state.top().is_none());
    }

    fn empty_store() -> crate::paint::geometry_store::GeometryStore {
        crate::paint::geometry_store::GeometryStore::new()
    }

    fn grid_scene(count: usize) -> (Scene, Vec<crate::paint::scene::LaneId>) {
        let mut scene = Scene::empty();
        let mut lanes = Vec::with_capacity(count);
        for index in 0..count {
            let lane = scene.new_lane();
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: LogicalRect::new(
                        px((index % 8) as f32 * 16.0),
                        px((index / 8) as f32 * 24.0),
                        px(10.0),
                        px(20.0)
                    ),
                    corner_radius: [px(0.0); 4],
                    paint: solid(),
                })
            ));
            lanes.push(lane);
        }
        scene.set_order(&lanes);
        (scene, lanes)
    }

    fn lane_instance_bytes(
        encoder: &InstanceEncoder,
        lane: crate::paint::scene::LaneId,
    ) -> Vec<u8> {
        let segment = encoder
            .buffer()
            .segments
            .get(lane.slot() as usize)
            .and_then(Option::as_deref)
            .expect("lane 必须有实例段");
        assert_eq!(segment.generation, lane.generation());
        encoder.buffer().as_bytes()[segment.start as usize * 128..segment.end as usize * 128]
            .to_vec()
    }

    #[test]
    fn an_unchanged_frame_reuses_every_segment() {
        let (scene, lanes) = grid_scene(8);
        let mut encoder = InstanceEncoder::new();
        encoder
            .encode(&scene, scale(), resolver(), &empty_store())
            .expect("初帧");
        assert_eq!(encoder.encoded_lanes(), 8);
        assert_eq!(encoder.reused_segments(), 0);
        let instances = encoder.buffer().as_bytes().to_vec();
        let ops = encoder.buffer().ops().to_vec();
        assert_eq!(encoder.buffer().instance_count(), 8);
        encoder.buffer_mut().clear_changes();

        encoder
            .encode(&scene, scale(), resolver(), &empty_store())
            .expect("空闲帧");
        assert_eq!(encoder.encoded_lanes(), 0, "无变化帧不得重编码任何 lane");
        assert_eq!(encoder.reused_segments(), lanes.len());
        assert_eq!(encoder.buffer().as_bytes(), instances.as_slice());
        assert_eq!(encoder.buffer().ops(), ops.as_slice());
        assert!(
            encoder.buffer_mut().changes().is_empty(),
            "无变化帧不得产生增量载荷"
        );
    }

    #[test]
    fn a_dirty_lane_only_reencodes_itself() {
        let (mut scene, lanes) = grid_scene(8);
        let mut encoder = InstanceEncoder::new();
        encoder
            .encode(&scene, scale(), resolver(), &empty_store())
            .expect("初帧");
        encoder.buffer_mut().clear_changes();

        let lane = lanes[3];
        let segment = encoder.buffer().segments[lane.slot() as usize]
            .as_deref()
            .expect("段存在")
            .range();
        assert!(scene.clear_lane(lane));
        assert!(scene.push(
            lane,
            Primitive::Quad(Quad {
                bounds: LogicalRect::new(px(1.0), px(2.0), px(10.0), px(20.0)),
                corner_radius: [px(0.0); 4],
                paint: solid(),
            })
        ));
        encoder
            .encode(&scene, scale(), resolver(), &empty_store())
            .expect("增量帧");

        assert_eq!(encoder.encoded_lanes(), 1, "只有脏 lane 被重编码");
        assert_eq!(
            encoder.written_bytes(),
            128,
            "只上传那一条 lane 的实例字节（1 个实例 = 128 B）"
        );
        assert_eq!(encoder.reused_segments(), lanes.len() - 1);
        assert_eq!(
            encoder.buffer_mut().changes(),
            &[segment],
            "变化区间 = 那条 lane 的段（实例数不变 ⇒ 原地重写）"
        );
    }

    #[test]
    fn incremental_and_full_encodes_agree_byte_for_byte() {
        let (mut scene, lanes) = grid_scene(6);
        let mut incremental = InstanceEncoder::new();
        let store = empty_store();
        incremental
            .encode(&scene, scale(), resolver(), &store)
            .expect("初帧");
        for (round, lane) in [lanes[2], lanes[4]].into_iter().enumerate() {
            assert!(scene.clear_lane(lane));
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: LogicalRect::new(px(1.0 + round as f32), px(2.0), px(10.0), px(20.0)),
                    corner_radius: [px(1.0); 4],
                    paint: solid(),
                })
            ));
            incremental
                .encode(&scene, scale(), resolver(), &store)
                .expect("增量帧");
        }

        let mut full = InstanceEncoder::new();
        full.encode(&scene, scale(), resolver(), &store)
            .expect("全量帧");

        assert_eq!(
            incremental.buffer().as_bytes(),
            full.buffer().as_bytes(),
            "全量与增量的实例环必须逐字节一致"
        );
        assert_eq!(incremental.buffer().ops(), full.buffer().ops());
        assert_eq!(
            incremental.buffer().instance_count(),
            full.buffer().instance_count()
        );
        for lane in &lanes {
            assert_eq!(
                lane_instance_bytes(&incremental, *lane),
                lane_instance_bytes(&full, *lane),
                "lane {lane:?} 的实例字节必须一致"
            );
        }
    }

    #[test]
    fn moved_lanes_keep_their_content() {
        let (mut scene, lanes) = grid_scene(4);
        let mut incremental = InstanceEncoder::new();
        let store = empty_store();
        incremental
            .encode(&scene, scale(), resolver(), &store)
            .expect("初帧");
        let lane = lanes[1];
        assert!(scene.clear_lane(lane));
        for index in 0..3 {
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: LogicalRect::new(px(index as f32), px(0.0), px(4.0), px(4.0)),
                    corner_radius: [px(0.0); 4],
                    paint: solid(),
                })
            ));
        }
        incremental
            .encode(&scene, scale(), resolver(), &store)
            .expect("增量帧");
        assert_eq!(incremental.encoded_lanes(), 1);

        let mut full = InstanceEncoder::new();
        full.encode(&scene, scale(), resolver(), &store)
            .expect("全量帧");
        for lane in &lanes {
            assert_eq!(
                lane_instance_bytes(&incremental, *lane),
                lane_instance_bytes(&full, *lane),
                "lane {lane:?} 的内容必须与全量编码一致"
            );
        }
    }

    #[test]
    fn overflowing_the_clip_table_forces_a_full_reencode() {
        let (mut scene, lanes) = grid_scene(3);
        let mut encoder = InstanceEncoder::new();
        let store = empty_store();
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("初帧");
        let filler = encoder.buffer().clips[0];
        while encoder.buffer().clips.len() <= super::CLIP_TABLE_LIMIT {
            encoder.buffer_mut().clips.push(filler);
        }
        let lane = lanes[0];
        assert!(scene.clear_lane(lane));
        assert!(scene.push(lane, quad_primitive(solid())));
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("重置帧");
        assert_eq!(
            encoder.encoded_lanes(),
            lanes.len(),
            "裁剪表重置必须让所有 lane 重编码"
        );
        assert_eq!(
            encoder.buffer().clips.len(),
            1,
            "重置后裁剪表只剩哨兵（本场景无 clip）"
        );
    }

    #[test]
    fn a_dpi_change_forces_a_full_reencode() {
        let (mut scene, lanes) = grid_scene(4);
        let mut encoder = InstanceEncoder::new();
        let store = empty_store();
        encoder
            .encode(&scene, DevicePixelRatio::ONE, resolver(), &store)
            .expect("1× 帧");
        let lane = lanes[2];
        assert!(scene.clear_lane(lane));
        assert!(scene.push(lane, quad_primitive(solid())));
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("2× 帧");
        assert_eq!(encoder.encoded_lanes(), lanes.len());
        let bytes = lane_instance_bytes(&encoder, lanes[0]);
        let instance: &[super::GpuPrimitive] = bytemuck::cast_slice(&bytes);
        assert_eq!(instance[0].bounds, [0.0, 0.0, 20.0, 40.0]);
    }

    #[test]
    fn compaction_packs_the_arena_on_a_clean_frame() {
        let (mut scene, lanes) = grid_scene(100);
        let mut encoder = InstanceEncoder::new();
        let store = empty_store();
        for lane in &lanes {
            assert!(scene.push(
                *lane,
                Primitive::Quad(Quad {
                    bounds: LogicalRect::new(px(8.0), px(0.0), px(4.0), px(4.0)),
                    corner_radius: [px(0.0); 4],
                    paint: solid(),
                })
            ));
        }
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("初帧");
        for lane in &lanes {
            assert!(scene.clear_lane(*lane));
            assert!(scene.push(
                *lane,
                Primitive::Quad(Quad {
                    bounds: LogicalRect::new(px(0.0), px(0.0), px(4.0), px(4.0)),
                    corner_radius: [px(0.0); 4],
                    paint: solid(),
                })
            ));
        }
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("搬段帧");
        let used_before = encoder.buffer().instance_count();
        let free_before: u32 = encoder
            .buffer()
            .free
            .iter()
            .map(|range| range.end - range.start)
            .sum();
        assert!(free_before >= 64, "100 条 lane 搬段必须留下足够碎片");

        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("压实帧");
        assert_eq!(encoder.encoded_lanes(), 0);
        assert!(encoder.buffer().free.is_empty(), "压实之后不得留下空闲块");
        let live: u32 = 100;
        assert_eq!(encoder.buffer().instance_count(), live);
        assert!(used_before > live, "压实前必须有空洞");
        assert_eq!(encoder.buffer().len() as u32, live);

        let mut full = InstanceEncoder::new();
        full.encode(&scene, scale(), resolver(), &store)
            .expect("全量帧");
        assert_eq!(encoder.buffer().as_bytes(), full.buffer().as_bytes());
        assert_eq!(encoder.buffer().ops(), full.buffer().ops());
    }

    #[test]
    fn op_bounds_are_local_and_ops_are_coalesced() {
        let (scene, _lanes) = grid_scene(200);
        let buffer = encode(&scene);
        let instances_ops: Vec<crate::math::DeviceRect> = buffer
            .ops
            .iter()
            .filter_map(|op| match op {
                super::FrameOp::Instances { bounds, .. } => Some(*bounds),
                super::FrameOp::Geometry { .. } => None,
            })
            .collect();
        assert_eq!(
            instances_ops.len(),
            200usize.div_ceil(super::MAX_LANES_PER_OP as usize),
            "连续 lane 必须合并成 op（不是一条 lane 一条 op）"
        );
        let first = instances_ops[0];
        assert_eq!(
            (first.x0.value(), first.y0.value()),
            (-2, -2),
            "包围盒含 AA 外扩 1 px + 取整余量 1 px"
        );
        assert!(
            first.x1 <= (8 * 16 + 10) * 2 + 2,
            "op bounds 不得扩散到整屏：{first:?}"
        );
        let bottom = instances_ops.last().expect("至少一条 op");
        let damage = crate::math::DeviceRect::new(bottom.x0, bottom.y0, bottom.x1, bottom.y1);
        assert!(
            !first.overlaps_inclusive(damage),
            "顶部 op 与底部 damage 不相交（op 级跳过生效）"
        );
    }
}

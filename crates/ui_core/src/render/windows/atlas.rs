use std::sync::{Mutex, MutexGuard, PoisonError};

use windows::Win32::Graphics::{
    Direct3D12::*,
    Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_UNKNOWN,
        DXGI_SAMPLE_DESC,
    },
};

use crate::{
    error::EngineError,
    paint::atlas::{AtlasKind, MAX_IMAGE_PAGES},
    render::{
        AtlasArraySpec, AtlasPageRequest,
        atlas::{DirtyRects, MAX_RESIDENT_STAGING_BYTES, RectChunk, texel_bytes},
        frame::MAX_FRAMES_IN_FLIGHT,
        merge_page_request,
        retire::GpuResourceHandle,
        windows::device::{SharedGpu, init_error_from},
    },
};

const PLACEHOLDER_PAGE: u32 = 1;

const STAGING_ROWS: u32 = 256;

const PITCH_ALIGNMENT: u32 = 256;

const PLACED_OFFSET_ALIGNMENT: u32 = 512;

struct SharedPage {
    resource: ID3D12Resource,
    page: u32,
    layers: u32,
    row_pitch: u32,
}

impl SharedPage {
    fn new(
        device: &ID3D12Device,
        kind: AtlasKind,
        spec: crate::render::AtlasArraySpec,
    ) -> Result<Self, EngineError> {
        let row_pitch = row_pitch_for(kind, spec.page);
        let resource = create_texture(device, spec.page, spec.layers, texture_format(kind))?;
        Ok(Self {
            resource,
            page: spec.page,
            layers: spec.layers,
            row_pitch,
        })
    }
}

pub(crate) struct SharedAtlas {
    glyph: SharedPage,
    image: SharedPage,
    shadow: [Mutex<Option<ShadowAtlas>>; 2],
}

struct ShadowAtlas {
    page: u32,
    layers: Vec<Option<Vec<u8>>>,
}

pub(crate) struct ShadowAtlasSnapshot {
    page: u32,
    layers: Vec<Option<Vec<u8>>>,
}

impl ShadowAtlasSnapshot {
    pub(crate) fn page(&self) -> u32 {
        self.page
    }

    pub(crate) fn layers(&self) -> &[Option<Vec<u8>>] {
        &self.layers
    }
}

impl SharedAtlas {
    pub(crate) fn new(device: &ID3D12Device) -> Result<Self, EngineError> {
        let placeholder = crate::render::AtlasArraySpec {
            page: PLACEHOLDER_PAGE,
            layers: 1,
        };
        Ok(Self {
            glyph: SharedPage::new(device, AtlasKind::Glyph, placeholder)?,
            image: SharedPage::new(device, AtlasKind::Image, placeholder)?,
            shadow: [Mutex::new(None), Mutex::new(None)],
        })
    }

    pub(crate) fn texture(&self, kind: AtlasKind) -> &ID3D12Resource {
        &self.page_ref(kind).resource
    }

    pub(crate) fn page(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).page
    }

    pub(crate) fn layers(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).layers
    }

    pub(crate) fn spec(&self, kind: AtlasKind) -> crate::render::AtlasArraySpec {
        let page = self.page_ref(kind);
        crate::render::AtlasArraySpec {
            page: page.page,
            layers: page.layers,
        }
    }

    #[cfg(test)]
    pub(crate) fn resource_array_size(&self, kind: AtlasKind) -> u16 {
        let desc = unsafe { self.page_ref(kind).resource.GetDesc() };
        desc.DepthOrArraySize
    }

    pub(crate) fn row_pitch(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).row_pitch
    }

    pub(crate) fn recreate(
        &mut self,
        device: &ID3D12Device,
        kind: AtlasKind,
        spec: crate::render::AtlasArraySpec,
    ) -> Result<ID3D12Resource, EngineError> {
        let next = SharedPage::new(device, kind, spec)?;
        let old = std::mem::replace(self.page_mut(kind), next);
        Ok(old.resource)
    }

    fn update_shadow(
        &self,
        kind: AtlasKind,
        layer: u16,
        page: u32,
        pixels: &[u8],
        rects: &DirtyRects,
    ) {
        let texel = texel_bytes(kind) as usize;
        let row_bytes = page as usize * texel;
        if row_bytes == 0 || pixels.len() != rects.byte_count(page, texel_bytes(kind)) {
            return;
        }
        let layers = self.layers(kind);
        let layer_index = usize::from(layer);
        let mut shadow = shadow_lock(&self.shadow[kind.index() as usize]);
        if shadow
            .as_ref()
            .is_none_or(|shadow| shadow.page != page || shadow.layers.len() != layers as usize)
        {
            *shadow = Some(ShadowAtlas {
                page,
                layers: (0..layers).map(|_| None).collect(),
            });
        }
        let Some(shadow) = shadow.as_mut() else {
            return;
        };
        let Some(layer_pixels) = shadow.layers.get_mut(layer_index) else {
            return;
        };
        if layer_pixels
            .as_ref()
            .is_none_or(|pixels| pixels.len() != row_bytes * page as usize)
        {
            *layer_pixels = Some(vec![0; row_bytes * page as usize]);
        }
        let Some(shadow_pixels) = layer_pixels.as_mut() else {
            return;
        };
        for chunk in rects.chunks(page, texel_bytes(kind)) {
            let width_bytes = chunk.width as usize * texel;
            for row in 0..chunk.height as usize {
                let source = chunk.offset + row * width_bytes;
                let destination = (chunk.y as usize + row) * row_bytes + chunk.x as usize * texel;
                if source + width_bytes <= pixels.len()
                    && destination + width_bytes <= shadow_pixels.len()
                {
                    shadow_pixels[destination..destination + width_bytes]
                        .copy_from_slice(&pixels[source..source + width_bytes]);
                }
            }
        }
    }

    pub(crate) fn shadow_snapshot(&self) -> [Option<ShadowAtlasSnapshot>; 2] {
        std::array::from_fn(|index| {
            shadow_lock(&self.shadow[index])
                .as_ref()
                .map(|shadow| ShadowAtlasSnapshot {
                    page: shadow.page,
                    layers: shadow.layers.clone(),
                })
        })
    }

    fn page_ref(&self, kind: AtlasKind) -> &SharedPage {
        match kind {
            AtlasKind::Glyph => &self.glyph,
            AtlasKind::Image => &self.image,
        }
    }

    fn page_mut(&mut self, kind: AtlasKind) -> &mut SharedPage {
        match kind {
            AtlasKind::Glyph => &mut self.glyph,
            AtlasKind::Image => &mut self.image,
        }
    }
}

fn shadow_lock(shadow: &Mutex<Option<ShadowAtlas>>) -> MutexGuard<'_, Option<ShadowAtlas>> {
    shadow.lock().unwrap_or_else(PoisonError::into_inner)
}

struct StagingSlot {
    buffer: ID3D12Resource,
    handle: GpuResourceHandle,
    capacity: u32,
    ptr: *mut u8,
}

unsafe impl Send for StagingSlot {}

impl StagingSlot {
    fn new_with_bytes(shared: &SharedGpu, bytes: u32) -> Result<Self, EngineError> {
        let buffer = create_buffer(
            shared.raw(),
            u64::from(bytes.max(1)),
            D3D12_HEAP_TYPE_UPLOAD,
        )?;
        let handle = shared.allocate(buffer.clone());
        let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe { buffer.Map(0, None, Some(&mut pointer)) }
            .map_err(|error| init_error_from("Map(atlas staging)", error))?;
        Ok(Self {
            buffer,
            handle,
            capacity: bytes,
            ptr: pointer.cast::<u8>(),
        })
    }
}

#[derive(Clone, Copy)]
struct StagingRect {
    pending: usize,
    chunk: RectChunk,
    layer: u16,
    row_pitch: u32,
    offset: u32,
}

struct AtlasStaging {
    kind: AtlasKind,
    page: u32,
    slots: Vec<StagingSlot>,
}

impl AtlasStaging {
    fn new(
        shared: &SharedGpu,
        kind: AtlasKind,
        page: u32,
        row_pitch: u32,
    ) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            slots.push(StagingSlot::new_with_bytes(
                shared,
                resident_bytes(row_pitch),
            )?);
        }
        Ok(Self { kind, page, slots })
    }

    fn retire(self, retired: &mut Vec<GpuResourceHandle>) {
        for slot in self.slots {
            retired.push(slot.handle);
        }
    }

    fn ensure_bytes(
        &mut self,
        shared: &SharedGpu,
        slot: usize,
        bytes: u32,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        if bytes <= self.slots[slot].capacity {
            return Ok(());
        }
        let staging = StagingSlot::new_with_bytes(shared, bytes)?;
        let old = std::mem::replace(&mut self.slots[slot], staging);
        retired.push(old.handle);
        Ok(())
    }

    fn layout(&self, pendings: &[PendingUpload]) -> (Vec<StagingRect>, u32) {
        let texel = texel_bytes(self.kind);
        let mut layout = Vec::new();
        let mut end = 0u32;
        for (pending_index, pending) in pendings.iter().enumerate() {
            for chunk in pending.rects.chunks(pending.spec.page, texel) {
                debug_assert!(
                    chunk.x + chunk.width <= self.page && chunk.y + chunk.height <= self.page,
                    "atlas AOI chunk {chunk:?} is outside the {}-texel page",
                    self.page
                );
                let row_pitch = align_up(chunk.width * texel, PITCH_ALIGNMENT);
                let offset = align_up(end, PLACED_OFFSET_ALIGNMENT);
                end = offset + row_pitch * chunk.height;
                layout.push(StagingRect {
                    pending: pending_index,
                    chunk,
                    layer: pending.layer,
                    row_pitch,
                    offset,
                });
            }
        }
        (layout, end)
    }

    fn upload(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
        pendings: &[PendingUpload],
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        let (layout, total_bytes) = self.layout(pendings);
        if layout.is_empty() {
            return Ok(());
        }
        let resource = atlas.texture(self.kind);
        transition(
            list,
            resource,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        let mut transient = None;
        if u64::from(total_bytes) <= MAX_RESIDENT_STAGING_BYTES {
            self.ensure_bytes(shared, slot, total_bytes, retired)?;
        } else {
            let temp = StagingSlot::new_with_bytes(shared, total_bytes)?;
            retired.push(temp.handle);
            transient = Some(temp);
        }
        let staging = transient.as_ref().unwrap_or(&self.slots[slot]);
        for rect in &layout {
            self.upload_rect(list, resource, staging, &pendings[rect.pending], *rect);
        }

        transition(
            list,
            resource,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        Ok(())
    }

    fn upload_rect(
        &self,
        list: &ID3D12GraphicsCommandList,
        resource: &ID3D12Resource,
        staging: &StagingSlot,
        pending: &PendingUpload,
        rect: StagingRect,
    ) {
        let texel = texel_bytes(self.kind) as usize;
        let width_bytes = rect.chunk.width as usize * texel;
        unsafe {
            for row in 0..rect.chunk.height as usize {
                let source = pending
                    .pixels
                    .as_ptr()
                    .add(rect.chunk.offset + row * width_bytes);
                let destination = staging
                    .ptr
                    .add(rect.offset as usize + row * rect.row_pitch as usize);
                std::ptr::copy_nonoverlapping(source, destination, width_bytes);
            }
        }

        let placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: u64::from(rect.offset),
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: texture_format(self.kind),
                Width: rect.chunk.width,
                Height: rect.chunk.height,
                Depth: 1,
                RowPitch: rect.row_pitch,
            },
        };
        let mut destination = D3D12_TEXTURE_COPY_LOCATION {
            pResource: core::mem::ManuallyDrop::new(Some(resource.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: u32::from(rect.layer),
            },
        };
        let mut source = D3D12_TEXTURE_COPY_LOCATION {
            pResource: core::mem::ManuallyDrop::new(Some(staging.buffer.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: placed,
            },
        };
        unsafe {
            list.CopyTextureRegion(&destination, rect.chunk.x, rect.chunk.y, 0, &source, None)
        };
        unsafe {
            core::mem::ManuallyDrop::drop(&mut destination.pResource);
            core::mem::ManuallyDrop::drop(&mut source.pResource);
        }
    }
}

struct PendingUpload {
    layer: u16,
    spec: AtlasArraySpec,
    queued_page: u32,
    pixels: Vec<u8>,
    rects: DirtyRects,
}

impl PendingUpload {
    fn covers_full_page(&self) -> bool {
        self.rects.covers_page(self.spec.page)
    }
}

pub(crate) struct AtlasUploads {
    pending: [[Option<PendingUpload>; MAX_IMAGE_PAGES as usize]; 2],
    staging: [Option<AtlasStaging>; 2],
}

impl AtlasUploads {
    pub(crate) fn new() -> Self {
        Self {
            pending: std::array::from_fn(|_| std::array::from_fn(|_| None)),
            staging: [None, None],
        }
    }

    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        self.pending[kind.index() as usize]
            .iter()
            .any(Option::is_some)
    }

    #[cfg(test)]
    fn staging(&self, kind: AtlasKind) -> Option<&AtlasStaging> {
        self.staging[kind.index() as usize].as_ref()
    }

    #[cfg(test)]
    pub(crate) fn pending_layers(&self, kind: AtlasKind) -> Vec<u16> {
        self.pending[kind.index() as usize]
            .iter()
            .enumerate()
            .filter_map(|(layer, slot)| {
                slot.as_ref()
                    .map(|_| u16::try_from(layer).expect("atlas layer index fits u16"))
            })
            .collect()
    }

    pub(crate) fn queue_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        spec: AtlasArraySpec,
        queued_page: u32,
        pixels: Vec<u8>,
        rects: DirtyRects,
    ) {
        assert!(
            u32::from(layer) < spec.layers,
            "atlas layer {layer} out of range for {kind:?}"
        );
        let page = spec.page;
        let expected = rects.byte_count(page, texel_bytes(kind));
        assert_eq!(
            pixels.len(),
            expected,
            "atlas payload size mismatch for {kind:?} layer {layer}: expected {expected} bytes for the dirty rects of a {page}² page, got {}",
            pixels.len()
        );
        let slot = &mut self.pending[kind.index() as usize][usize::from(layer)];
        assert!(
            slot.is_none(),
            "atlas payload slot must be consumed before the next upload (kind {kind:?}, layer {layer})"
        );
        *slot = Some(PendingUpload {
            layer,
            spec,
            queued_page,
            pixels,
            rects,
        });
    }

    pub(crate) fn reconcile_pages(
        &mut self,
        current: [AtlasArraySpec; 2],
    ) -> Result<Vec<AtlasPageRequest>, EngineError> {
        let mut rebuild: Vec<AtlasPageRequest> = Vec::with_capacity(2);
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let current = current[index];
            let pending_spec = self.pending[index]
                .iter()
                .flatten()
                .map(|pending| pending.spec)
                .fold(None, |acc: Option<AtlasArraySpec>, spec| {
                    Some(match acc {
                        Some(acc) => AtlasArraySpec {
                            page: spec.page,
                            layers: acc.layers.max(spec.layers),
                        },
                        None => spec,
                    })
                });
            if let Some(pending_spec) = pending_spec
                && pending_spec.layers > current.layers
            {
                for layer in 0..pending_spec.layers {
                    let full = self.pending[index]
                        .get(layer as usize)
                        .and_then(Option::as_ref)
                        .is_some_and(|pending| {
                            pending.spec.page == pending_spec.page && pending.covers_full_page()
                        });
                    if !full {
                        return Err(EngineError::AtlasGrowthIncomplete {
                            kind,
                            layer: u16::try_from(layer).unwrap_or(u16::MAX),
                            layers: pending_spec.layers,
                        });
                    }
                }
            }
            for layer in 0..self.pending[index].len() {
                let Some(pending) = &self.pending[index][layer] else {
                    continue;
                };
                let spec = pending.spec;
                if spec.page == current.page {
                    if spec.layers <= current.layers {
                        continue;
                    }
                    if !pending.covers_full_page() {
                        self.pending[index][layer] = None;
                        continue;
                    }
                    merge_page_request(&mut rebuild, kind, spec, current);
                    continue;
                }
                if pending.covers_full_page() && pending.queued_page != spec.page {
                    merge_page_request(&mut rebuild, kind, spec, current);
                } else {
                    self.pending[index][layer] = None;
                }
            }
        }
        Ok(rebuild)
    }

    pub(crate) fn release(&self, shared: &SharedGpu) {
        for staging in self.staging.iter().flatten() {
            for slot in &staging.slots {
                let _ = shared.release(slot.handle);
            }
        }
    }

    pub(crate) fn sync_staging(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let Some(page) = self.pending[index]
                .iter()
                .flatten()
                .map(|pending| pending.spec.page)
                .next()
            else {
                continue;
            };
            debug_assert!(
                self.pending[index]
                    .iter()
                    .flatten()
                    .all(|pending| pending.spec.page == page),
                "all layers of one atlas must share the same page extent"
            );
            if self.staging[index]
                .as_ref()
                .is_some_and(|staging| staging.page == page)
            {
                continue;
            }
            let staging = AtlasStaging::new(shared, kind, page, atlas.row_pitch(kind))?;
            if let Some(old) = self.staging[index].replace(staging) {
                old.retire(retired);
            }
            debug_assert_eq!(atlas.page(kind), page, "图集页与待上传页必须已经对齐");
        }
        Ok(())
    }

    pub(crate) fn record_uploads(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
    ) -> Result<Vec<GpuResourceHandle>, EngineError> {
        let mut retired = Vec::new();
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let pendings: Vec<PendingUpload> = self.pending[index]
                .iter_mut()
                .filter_map(Option::take)
                .collect();
            if pendings.is_empty() {
                continue;
            }
            let page = atlas.page(kind);
            if pendings.iter().any(|pending| pending.spec.page != page) {
                continue;
            }
            debug_assert!(
                pendings
                    .iter()
                    .all(|pending| u32::from(pending.layer) < atlas.layers(kind)),
                "pending layers must be inside the shared texture array"
            );
            for pending in &pendings {
                atlas.update_shadow(
                    kind,
                    pending.layer,
                    pending.spec.page,
                    &pending.pixels,
                    &pending.rects,
                );
            }
            let Some(staging) = self.staging[index].as_mut() else {
                return Err(EngineError::AppInit {
                    detail: smol_str::format_smolstr!(
                        "atlas staging missing for {kind:?} page {page}"
                    ),
                });
            };
            debug_assert_eq!(staging.page, page);
            staging.upload(shared, atlas, list, slot, &pendings, &mut retired)?;
        }
        Ok(retired)
    }
}

pub(crate) fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: core::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: core::mem::ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    unsafe { list.ResourceBarrier(core::slice::from_ref(&barrier)) };
    unsafe {
        let transition: &mut D3D12_RESOURCE_TRANSITION_BARRIER = &mut barrier.Anonymous.Transition;
        core::mem::ManuallyDrop::drop(&mut transition.pResource);
    }
}

fn texture_format(kind: AtlasKind) -> windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
    match kind {
        AtlasKind::Glyph => DXGI_FORMAT_R8_UNORM,
        AtlasKind::Image => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    }
}

fn align_up(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

pub(crate) fn row_pitch_for(kind: AtlasKind, page: u32) -> u32 {
    align_up(page * texel_bytes(kind), PITCH_ALIGNMENT)
}

pub(crate) fn copy_page(
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    staging: &ID3D12Resource,
    kind: AtlasKind,
    page: u32,
    layer: u16,
    row_pitch: u32,
) {
    let placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            Format: texture_format(kind),
            Width: page,
            Height: page,
            Depth: 1,
            RowPitch: row_pitch,
        },
    };
    let mut destination = D3D12_TEXTURE_COPY_LOCATION {
        pResource: core::mem::ManuallyDrop::new(Some(texture.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: u32::from(layer),
        },
    };
    let mut source = D3D12_TEXTURE_COPY_LOCATION {
        pResource: core::mem::ManuallyDrop::new(Some(staging.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: placed,
        },
    };
    unsafe { list.CopyTextureRegion(&destination, 0, 0, 0, &source, None) };
    unsafe {
        core::mem::ManuallyDrop::drop(&mut destination.pResource);
        core::mem::ManuallyDrop::drop(&mut source.pResource);
    }
}

fn resident_bytes(row_pitch: u32) -> u32 {
    (u64::from(row_pitch) * u64::from(STAGING_ROWS)).min(MAX_RESIDENT_STAGING_BYTES) as u32
}

fn create_texture(
    device: &ID3D12Device,
    page: u32,
    layers: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<ID3D12Resource, EngineError> {
    debug_assert!(layers > 0, "texture array must have at least one layer");
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(page),
        Height: page,
        DepthOrArraySize: u16::try_from(layers).expect("atlas texture array layer count fits u16"),
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_NONE,
        ..D3D12_RESOURCE_DESC::default()
    };
    create_resource(
        device,
        &desc,
        D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
    )
}

fn create_buffer(
    device: &ID3D12Device,
    bytes: u64,
    heap_type: D3D12_HEAP_TYPE,
) -> Result<ID3D12Resource, EngineError> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.max(4),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..D3D12_RESOURCE_DESC::default()
    };
    create_resource(device, &desc, heap_type, D3D12_RESOURCE_STATE_GENERIC_READ)
}

fn create_resource(
    device: &ID3D12Device,
    desc: &D3D12_RESOURCE_DESC,
    heap_type: D3D12_HEAP_TYPE,
    initial: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource, EngineError> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: heap_type,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let mut out: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, desc, initial, None, &mut out)
    }
    .map_err(|error| init_error_from("CreateCommittedResource", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 CreateCommittedResource returned no resource"),
    })
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::{
        Direct3D12::D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8_UNORM},
    };

    use super::{AtlasUploads, STAGING_ROWS, SharedAtlas, align_up, resident_bytes};
    use crate::{
        math::PhysicalRect,
        paint::atlas::AtlasKind,
        render::{
            AtlasArraySpec, AtlasPageRequest,
            atlas::{DirtyRects, MAX_ATLAS_PAGE, MAX_RESIDENT_STAGING_BYTES},
            frame::MAX_FRAMES_IN_FLIGHT,
            windows::{device::SharedGpu, queue::CommandStream, readback::Readback},
        },
    };

    const TEST_PAGE: u32 = 64;

    fn full_rects() -> DirtyRects {
        DirtyRects::full()
    }

    fn rows(y0: u32, y1: u32) -> DirtyRects {
        let mut rects = DirtyRects::default();
        rects.mark(PhysicalRect::new(0, y0 as i32, i32::MAX, y1 as i32));
        rects
    }

    fn spec(page: u32, layers: u32) -> AtlasArraySpec {
        AtlasArraySpec { page, layers }
    }

    fn request(kind: AtlasKind, page: u32, layers: u32) -> AtlasPageRequest {
        AtlasPageRequest {
            kind,
            spec: spec(page, layers),
        }
    }

    fn rect(x0: u32, y0: u32, x1: u32, y1: u32) -> DirtyRects {
        let mut rects = DirtyRects::default();
        rects.mark(PhysicalRect::new(
            x0 as i32, y0 as i32, x1 as i32, y1 as i32,
        ));
        rects
    }

    struct Harness {
        shared: std::sync::Arc<SharedGpu>,
        stream: CommandStream,
        atlas: SharedAtlas,
        uploads: AtlasUploads,
    }

    impl Harness {
        fn new() -> Self {
            let shared = SharedGpu::new_warp().expect("WARP shared gpu");
            let stream = CommandStream::new(&shared).expect("command stream");
            let atlas = SharedAtlas::new(shared.raw()).expect("shared atlas");
            Self {
                shared,
                stream,
                atlas,
                uploads: AtlasUploads::new(),
            }
        }

        fn queue(
            &mut self,
            kind: AtlasKind,
            layer: u16,
            spec: AtlasArraySpec,
            pixels: Vec<u8>,
            dirty: DirtyRects,
        ) {
            let queued_page = self.atlas.page(kind);
            self.uploads
                .queue_upload(kind, layer, spec, queued_page, pixels, dirty);
        }

        fn flush(&mut self) -> Vec<crate::render::retire::GpuResourceHandle> {
            let mut rebuild = self
                .uploads
                .reconcile_pages([
                    self.atlas.spec(AtlasKind::Glyph),
                    self.atlas.spec(AtlasKind::Image),
                ])
                .expect("reconcile pages");
            for request in rebuild.drain(..) {
                let _old = self
                    .atlas
                    .recreate(self.shared.raw(), request.kind, request.spec)
                    .expect("recreate atlas array");
            }
            let mut retired = Vec::new();
            self.uploads
                .sync_staging(&self.shared, &self.atlas, &mut retired)
                .expect("sync staging");
            let shared = &self.shared;
            let atlas = &self.atlas;
            let uploads = &mut self.uploads;
            self.stream
                .submit_immediate(|list| {
                    retired.extend(uploads.record_uploads(shared, atlas, list, 0)?);
                    Ok(())
                })
                .expect("upload");
            retired
        }

        fn upload(
            &mut self,
            kind: AtlasKind,
            layer: u16,
            spec: AtlasArraySpec,
            pixels: Vec<u8>,
            dirty: DirtyRects,
        ) -> Vec<crate::render::retire::GpuResourceHandle> {
            self.queue(kind, layer, spec, pixels, dirty);
            self.flush()
        }

        fn readback(&mut self, kind: AtlasKind, page: u32, layer: u16) -> ReadbackImage {
            let format = match kind {
                AtlasKind::Glyph => DXGI_FORMAT_R8_UNORM,
                AtlasKind::Image => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            };
            let mut readback = Readback::new();
            readback
                .capture_from(
                    &self.shared,
                    &mut self.stream,
                    self.atlas.texture(kind),
                    (page, page),
                    format,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                    layer,
                )
                .expect("readback")
        }
    }

    type ReadbackImage = crate::render::windows::readback::ReadbackImage;

    #[test]
    fn row_pitch_is_256_aligned_for_both_texels() {
        assert_eq!(align_up(MAX_ATLAS_PAGE, 256), 2048);
        assert_eq!(align_up(MAX_ATLAS_PAGE * 4, 256), 8192);
    }

    #[test]
    fn resident_staging_stays_within_the_cap() {
        for pitch in [256u32, 512, 1024, 2048, 4096, 8192] {
            let bytes = resident_bytes(pitch);
            assert!(bytes > 0, "pitch {pitch}: 常驻容量不得为 0");
            assert!(
                u64::from(bytes) <= MAX_RESIDENT_STAGING_BYTES,
                "pitch {pitch}: {bytes} bytes"
            );
        }
        assert_eq!(resident_bytes(8192), MAX_RESIDENT_STAGING_BYTES as u32);
        assert_eq!(resident_bytes(256), STAGING_ROWS * 256);
    }

    #[test]
    fn reconcile_pages_keeps_full_page_rebuilds_and_drops_stale_increments() {
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            TEST_PAGE,
            vec![0xAB; 2 * TEST_PAGE as usize],
            rows(4, 6),
        );
        assert!(
            uploads
                .reconcile_pages([spec(TEST_PAGE * 8, 1), spec(1, 1)])
                .expect("reconcile pages")
                .is_empty(),
            "非整页的旧页增量不应触发重建"
        );
        assert!(
            !uploads.pending(AtlasKind::Glyph),
            "过期增量必须在页对齐时被丢弃"
        );

        uploads.queue_upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            1,
            vec![0u8; (TEST_PAGE * TEST_PAGE) as usize],
            full_rects(),
        );
        assert_eq!(
            uploads
                .reconcile_pages([spec(TEST_PAGE * 8, 1), spec(1, 1)])
                .expect("reconcile pages"),
            vec![request(AtlasKind::Glyph, TEST_PAGE, 1)],
            "整页上传必须要求把共享 array 重建到它自己的页尺寸"
        );
        assert!(
            uploads
                .reconcile_pages([spec(TEST_PAGE, 1), spec(1, 1)])
                .expect("reconcile pages")
                .is_empty(),
            "页已对齐时不再要求重建"
        );
        assert!(uploads.pending(AtlasKind::Glyph));
    }

    #[test]
    fn reconcile_pages_deduplicates_the_rebuild_request_for_every_layer() {
        let mut uploads = AtlasUploads::new();
        for layer in 0..4u16 {
            uploads.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 4),
                1,
                vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
                full_rects(),
            );
        }
        assert_eq!(
            uploads
                .reconcile_pages([spec(1, 1), spec(1, 1)])
                .expect("reconcile pages"),
            vec![request(AtlasKind::Image, TEST_PAGE, 4)],
            "按 extent 去重：数组只有一次重建，且层数取最大值"
        );
        assert!(uploads.pending(AtlasKind::Image));
        assert!(
            (0..4).all(|layer| {
                let kind = AtlasKind::Image;
                uploads.pending(kind) && uploads.pending[kind.index() as usize][layer].is_some()
            }),
            "四层都要保留到重建之后上传"
        );
    }

    #[test]
    fn reconcile_pages_discards_a_full_page_upload_of_a_superseded_page() {
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            0,
            spec(512, 1),
            512,
            vec![0u8; 512 * 512],
            full_rects(),
        );
        assert!(
            uploads
                .reconcile_pages([spec(1024, 1), spec(1, 1)])
                .expect("reconcile pages")
                .is_empty(),
            "旧页整页上传不得触发重建"
        );
        assert!(!uploads.pending(AtlasKind::Glyph), "旧页上传必须被丢弃");
    }

    #[test]
    fn a_full_page_upload_does_not_grow_the_resident_slot() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        const PAGE: u32 = 1024;
        let retired = harness.upload(
            AtlasKind::Image,
            0,
            spec(PAGE, 1),
            vec![0u8; (PAGE * PAGE * 4) as usize],
            full_rects(),
        );
        assert!(
            !retired.is_empty(),
            "整页上传必须把一次性 staging 交给 retire"
        );
        let staging = harness
            .uploads
            .staging(AtlasKind::Image)
            .expect("image staging");
        assert_eq!(staging.page, PAGE);
        let resident = staging.slots[0].capacity;
        assert!(
            u64::from(resident) <= MAX_RESIDENT_STAGING_BYTES,
            "常驻槽被整页上传撑大：{resident} B"
        );
        assert!(
            u64::from(resident) < u64::from(PAGE) * u64::from(PAGE) * 4,
            "整页字节数不得常驻（{resident} B）"
        );
    }

    #[test]
    fn a_dirty_row_upload_only_touches_that_row() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            vec![0u8; (TEST_PAGE * TEST_PAGE) as usize],
            full_rects(),
        );
        let page = vec![0xABu8; TEST_PAGE as usize];
        harness.upload(AtlasKind::Glyph, 0, spec(TEST_PAGE, 1), page, rows(5, 6));

        let image = harness.readback(AtlasKind::Glyph, TEST_PAGE, 0);
        assert_eq!(image.rgba8[(5 * TEST_PAGE * 4) as usize], 0xAB);
        assert_eq!(image.rgba8[(5 * TEST_PAGE * 4 + 3) as usize], 0xFF);
        assert_eq!(image.rgba8[(4 * TEST_PAGE * 4) as usize], 0);
        assert_eq!(image.rgba8[(6 * TEST_PAGE * 4) as usize], 0);
    }

    #[test]
    fn an_eight_by_eleven_glyph_uploads_only_its_aoi_rect() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            vec![0u8; (TEST_PAGE * TEST_PAGE) as usize],
            full_rects(),
        );
        const X: u32 = 20;
        const Y: u32 = 30;
        const W: u32 = 8;
        const H: u32 = 11;
        let glyph = rect(X, Y, X + W, Y + H);
        assert_eq!(glyph.byte_count(TEST_PAGE, 1), 88, "载荷必须只有 88 B");
        assert_eq!(11 * TEST_PAGE as usize, 704, "旧语义要传 11 整行 × 页宽 64");
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            vec![0xCDu8; 88],
            glyph,
        );

        let image = harness.readback(AtlasKind::Glyph, TEST_PAGE, 0);
        let texel = |x: u32, y: u32| image.rgba8[((y * TEST_PAGE + x) * 4) as usize];
        assert_eq!(texel(X, Y), 0xCD, "AOI 左上角必须被写");
        assert_eq!(
            texel(X + W - 1, Y + H - 1),
            0xCD,
            "AOI 右下角必须被写（行内只有 width × texel 字节）"
        );
        assert_eq!(texel(X - 1, Y), 0, "AOI 左侧一列不得被写");
        assert_eq!(texel(X + W, Y), 0, "AOI 右侧一列不得被写");
        assert_eq!(texel(X, Y - 1), 0, "AOI 上一行不得被写");
        assert_eq!(texel(X, Y + H), 0, "AOI 下一行不得被写");
    }

    #[test]
    fn queueing_a_dirty_span_marks_pending() {
        let mut uploads = AtlasUploads::new();
        assert!(!uploads.pending(AtlasKind::Glyph));
        uploads.queue_upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            TEST_PAGE,
            vec![0x7Fu8; 2 * TEST_PAGE as usize],
            rows(4, 6),
        );
        assert!(uploads.pending(AtlasKind::Glyph));
        assert!(!uploads.pending(AtlasKind::Image));
    }

    #[test]
    fn the_image_array_is_lazy_and_starts_with_one_layer() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        fn array_bytes(harness: &Harness) -> u64 {
            let desc = unsafe { harness.atlas.texture(AtlasKind::Image).GetDesc() };
            assert_eq!(
                u64::from(desc.Height),
                desc.Width,
                "图集纹理必须是正方形（页边长一致）"
            );
            desc.Width * u64::from(desc.Height) * u64::from(desc.DepthOrArraySize) * 4
        }
        let array_layers = |harness: &Harness| {
            unsafe { harness.atlas.texture(AtlasKind::Image).GetDesc() }.DepthOrArraySize
        };

        assert_eq!(harness.atlas.page(AtlasKind::Image), 1, "占位页是 1×1");
        assert_eq!(
            harness.atlas.layers(AtlasKind::Image),
            1,
            "占位数组只有 1 层"
        );
        assert_eq!(array_layers(&harness), 1, "占位纹理是 1×1×1 数组");
        assert_eq!(array_bytes(&harness), 4, "占位数组只占 4 B");
        assert!(
            array_bytes(&harness) < 16 * 1024 * 1024,
            "没有图片时图片图集不得占 16 MiB"
        );

        harness.upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            vec![0x11u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
            full_rects(),
        );
        assert_eq!(harness.atlas.page(AtlasKind::Image), TEST_PAGE);
        assert_eq!(harness.atlas.layers(AtlasKind::Image), 1, "1 页 ⇒ 1 层");
        assert_eq!(array_layers(&harness), 1, "1 页的纹理数组必须是 1 层");
        assert_eq!(
            array_bytes(&harness),
            u64::from(TEST_PAGE) * u64::from(TEST_PAGE) * 4,
            "1 页只建 1 层（不是 4 层）"
        );
        let single_layer_bytes = array_bytes(&harness);
        assert!(
            single_layer_bytes < 64 * 1024 * 1024,
            "单层 {single_layer_bytes} B ≪ 旧实现的 4 层 64 MiB"
        );
    }

    #[test]
    fn growing_the_image_array_keeps_the_existing_layer_content() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        const LAYER_ZERO: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];
        const LAYER_ONE: [u8; 4] = [0x44, 0x55, 0x66, 0xFF];
        fn fill(colour: [u8; 4]) -> Vec<u8> {
            let mut pixels = vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize];
            for texel in pixels.as_chunks_mut::<4>().0 {
                *texel = colour;
            }
            pixels
        }
        fn first_texel(harness: &mut Harness, layer: u16) -> [u8; 4] {
            let image = harness.readback(AtlasKind::Image, TEST_PAGE, layer);
            image.rgba8[..4].try_into().expect("4 bytes")
        }
        const ZERO_RGBA: [u8; 4] = [LAYER_ZERO[2], LAYER_ZERO[1], LAYER_ZERO[0], LAYER_ZERO[3]];
        const ONE_RGBA: [u8; 4] = [LAYER_ONE[2], LAYER_ONE[1], LAYER_ONE[0], LAYER_ONE[3]];

        harness.upload(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 1),
            fill(LAYER_ZERO),
            full_rects(),
        );
        assert_eq!(harness.atlas.layers(AtlasKind::Image), 1);
        assert_eq!(first_texel(&mut harness, 0), ZERO_RGBA);

        harness.queue(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 2),
            fill(LAYER_ZERO),
            full_rects(),
        );
        harness.queue(
            AtlasKind::Image,
            1,
            spec(TEST_PAGE, 2),
            fill(LAYER_ONE),
            full_rects(),
        );
        harness.flush();

        assert_eq!(
            harness.atlas.layers(AtlasKind::Image),
            2,
            "页数 1 → 2 ⇒ texture array 必须重建为 2 层"
        );
        let desc = unsafe { harness.atlas.texture(AtlasKind::Image).GetDesc() };
        assert_eq!(desc.DepthOrArraySize, 2, "重建后的纹理是 2 层数组");
        assert_eq!(
            desc.Width * u64::from(desc.Height) * u64::from(desc.DepthOrArraySize) * 4,
            u64::from(TEST_PAGE) * u64::from(TEST_PAGE) * 4 * 2,
            "2 层数组的字节数 = 2 × 单层"
        );
        assert_eq!(
            first_texel(&mut harness, 0),
            ZERO_RGBA,
            "重建后第 0 层内容必须仍在（增长不能丢已上传的图）"
        );
        assert_eq!(
            first_texel(&mut harness, 1),
            ONE_RGBA,
            "第 1 层（新页）内容必须正确"
        );
    }

    #[test]
    fn image_atlas_layers_upload_to_their_own_array_slices() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        const COLORS: [[u8; 4]; 4] = [
            [0x11, 0x22, 0x33, 0xFF],
            [0x44, 0x55, 0x66, 0xFF],
            [0x77, 0x88, 0x99, 0xFF],
            [0xAA, 0xBB, 0xCC, 0xFF],
        ];
        for (layer, color) in COLORS.iter().enumerate() {
            let mut pixels = vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize];
            for texel in pixels.as_chunks_mut::<4>().0 {
                *texel = *color;
            }
            harness.queue(
                AtlasKind::Image,
                layer as u16,
                spec(TEST_PAGE, 4),
                pixels,
                full_rects(),
            );
        }
        harness.flush();
        assert_eq!(
            harness.atlas.layers(AtlasKind::Image),
            4,
            "请求 4 层 ⇒ texture array 必须建成 4 层"
        );

        for (layer, color) in COLORS.iter().enumerate() {
            let image = harness.readback(AtlasKind::Image, TEST_PAGE, layer as u16);
            assert_eq!(
                [
                    image.rgba8[0],
                    image.rgba8[1],
                    image.rgba8[2],
                    image.rgba8[3]
                ],
                [color[2], color[1], color[0], color[3]],
                "第 {layer} 层的首像素必须来自该层的上传（BGRA→RGBA）"
            );
            let last = ((TEST_PAGE * TEST_PAGE - 1) * 4) as usize;
            assert_eq!(
                [
                    image.rgba8[last],
                    image.rgba8[last + 1],
                    image.rgba8[last + 2],
                    image.rgba8[last + 3]
                ],
                [color[2], color[1], color[0], color[3]],
                "第 {layer} 层的末像素必须来自该层的上传"
            );
        }
    }

    #[test]
    #[should_panic(expected = "atlas layer 2 out of range")]
    fn queueing_an_out_of_range_image_layer_is_a_contract_violation() {
        let mut uploads = AtlasUploads::new();
        let rects = rows(0, 1);
        let bytes = rects.byte_count(TEST_PAGE, 4);
        uploads.queue_upload(
            AtlasKind::Image,
            2,
            spec(TEST_PAGE, 2),
            TEST_PAGE,
            vec![0u8; bytes],
            rects,
        );
    }

    const BIG_PAGE: u32 = 512;

    #[test]
    fn cross_slot_staging_capacity_is_per_slot() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();

        let glyph = vec![0x11u8; (BIG_PAGE * BIG_PAGE) as usize];
        harness.upload(AtlasKind::Glyph, 0, spec(BIG_PAGE, 1), glyph, full_rects());

        let mut image = vec![0u8; (BIG_PAGE * BIG_PAGE * 4) as usize];
        for texel in image.as_chunks_mut::<4>().0 {
            *texel = [0x33, 0x44, 0x55, 0xFF];
        }
        harness.upload(AtlasKind::Image, 0, spec(BIG_PAGE, 1), image, full_rects());

        let glyph_image = harness.readback(AtlasKind::Glyph, BIG_PAGE, 0);
        let glyph_row = |row: u32| glyph_image.rgba8[(row * BIG_PAGE * 4) as usize];
        assert_eq!(glyph_row(0), 0x11, "glyph row 0");
        assert_eq!(glyph_row(BIG_PAGE - 1), 0x11, "glyph last row");

        let image_image = harness.readback(AtlasKind::Image, BIG_PAGE, 0);
        let image_row = |row: u32| {
            let index = (row * BIG_PAGE * 4) as usize;
            [
                image_image.rgba8[index],
                image_image.rgba8[index + 1],
                image_image.rgba8[index + 2],
                image_image.rgba8[index + 3],
            ]
        };
        assert_eq!(image_row(0), [0x55, 0x44, 0x33, 0xFF], "image row 0");
        assert_eq!(
            image_row(BIG_PAGE - 1),
            [0x55, 0x44, 0x33, 0xFF],
            "image last row"
        );
    }

    #[test]
    fn a_page_change_retires_the_old_staging_instead_of_releasing_it() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        let _ = harness.upload(
            AtlasKind::Glyph,
            0,
            spec(64, 1),
            vec![0u8; (64 * 64) as usize],
            full_rects(),
        );
        let live = harness.shared.live_resources();
        assert_eq!(live, MAX_FRAMES_IN_FLIGHT, "页 64 只注册 3 个常驻 staging");

        let retired = harness.upload(
            AtlasKind::Glyph,
            0,
            spec(128, 1),
            vec![0u8; (128 * 128) as usize],
            full_rects(),
        );
        assert_eq!(
            retired.len(),
            MAX_FRAMES_IN_FLIGHT,
            "整组 3 个旧 staging 必须交给 retire（R08-2）"
        );
        assert_eq!(
            harness.shared.live_resources(),
            live + MAX_FRAMES_IN_FLIGHT,
            "新增 3 个 staging，而旧的 3 个必须**还活着**（尚未 retire）"
        );

        for handle in retired {
            harness.stream.retire(1, handle);
        }
        assert!(
            harness.stream.pending_retire() >= MAX_FRAMES_IN_FLIGHT,
            "retire 队列必须持有这些句柄直到该帧 fence 完成"
        );
    }

    #[test]
    fn the_atlas_shadow_mirrors_full_and_incremental_uploads() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        let row_bytes = TEST_PAGE as usize;

        let mut page = vec![0u8; row_bytes * TEST_PAGE as usize];
        page[5 * row_bytes + 3] = 0xAB;
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            page.clone(),
            full_rects(),
        );
        let snapshot = harness.atlas.shadow_snapshot();
        let shadow = snapshot[0]
            .as_ref()
            .expect("glyph shadow after full upload");
        assert_eq!(shadow.page, TEST_PAGE);
        assert_eq!(
            shadow.layers[0].as_deref(),
            Some(page.as_slice()),
            "影子页必须与整页字节 1:1"
        );

        let dirty_row = vec![0x77u8; row_bytes];
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            dirty_row,
            rows(5, 6),
        );
        let snapshot = harness.atlas.shadow_snapshot();
        let bytes = snapshot[0].as_ref().expect("glyph shadow").layers[0]
            .as_ref()
            .expect("glyph layer shadow");
        assert_eq!(bytes[5 * row_bytes + 3], 0x77, "脏行被覆盖");
        assert_eq!(bytes[4 * row_bytes], 0, "脏行之外保持不变");

        let aoi = rect(3, 7, 7, 9);
        assert_eq!(aoi.byte_count(TEST_PAGE, 1), 8);
        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(TEST_PAGE, 1),
            vec![0x99u8; 8],
            aoi,
        );
        let snapshot = harness.atlas.shadow_snapshot();
        let bytes = snapshot[0].as_ref().expect("glyph shadow").layers[0]
            .as_ref()
            .expect("glyph layer shadow");
        assert_eq!(bytes[7 * row_bytes + 3], 0x99, "AOI 左上角");
        assert_eq!(bytes[8 * row_bytes + 6], 0x99, "AOI 右下角");
        assert_eq!(bytes[7 * row_bytes + 2], 0, "AOI 左侧一列保持原样");
        assert_eq!(bytes[7 * row_bytes + 7], 0, "AOI 右侧一列保持原样");
        assert_eq!(bytes[6 * row_bytes + 3], 0, "AOI 上一行保持原样");
        assert_eq!(bytes[9 * row_bytes + 3], 0, "AOI 下一行保持原样");

        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(128, 1),
            vec![0x11u8; 128 * 128],
            full_rects(),
        );
        let snapshot = harness.atlas.shadow_snapshot();
        let shadow = snapshot[0]
            .as_ref()
            .expect("glyph shadow after page change");
        assert_eq!(shadow.page, 128);
        let bytes = shadow.layers[0].as_ref().expect("glyph layer shadow");
        assert_eq!(bytes.len(), 128 * 128);
        assert!(bytes.iter().all(|byte| *byte == 0x11));
    }

    #[test]
    fn the_image_atlas_shadow_is_tracked_per_layer() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        let _grown = harness
            .atlas
            .recreate(harness.shared.raw(), AtlasKind::Image, spec(TEST_PAGE, 3))
            .expect("grow image array to 3 layers");
        harness.queue(
            AtlasKind::Image,
            0,
            spec(TEST_PAGE, 3),
            vec![0x11u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
            full_rects(),
        );
        harness.queue(
            AtlasKind::Image,
            2,
            spec(TEST_PAGE, 3),
            vec![0x22u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
            full_rects(),
        );
        harness.flush();
        let snapshot = harness.atlas.shadow_snapshot();
        let shadow = snapshot[1].as_ref().expect("image shadow");
        assert_eq!(shadow.page, TEST_PAGE);
        assert_eq!(
            shadow.layers.len(),
            3,
            "影子层数 = 数组当前层数（请求 3 层）"
        );
        assert!(
            shadow.layers[0]
                .as_ref()
                .is_some_and(|bytes| bytes.iter().all(|byte| *byte == 0x11))
        );
        assert!(shadow.layers[1].is_none(), "未上传的层没有影子");
        assert!(
            shadow.layers[2]
                .as_ref()
                .is_some_and(|bytes| bytes.iter().all(|byte| *byte == 0x22))
        );
    }

    #[test]
    fn multi_rect_upload_places_each_rect_at_its_own_offset() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();

        harness.upload(
            AtlasKind::Glyph,
            0,
            spec(BIG_PAGE, 1),
            vec![0u8; (BIG_PAGE * BIG_PAGE) as usize],
            full_rects(),
        );

        let mut page = vec![0x44u8; (10 * BIG_PAGE) as usize];
        page.extend(std::iter::repeat_n(0x55u8, (20 * BIG_PAGE) as usize));
        let mut dirty = DirtyRects::default();
        dirty.mark(PhysicalRect::new(0, 10, i32::MAX, 20));
        dirty.mark(PhysicalRect::new(0, 300, i32::MAX, 320));
        harness.upload(AtlasKind::Glyph, 0, spec(BIG_PAGE, 1), page, dirty);

        let image = harness.readback(AtlasKind::Glyph, BIG_PAGE, 0);
        let row = |row: u32| image.rgba8[(row * BIG_PAGE * 4) as usize];
        assert_eq!(row(0), 0, "untouched row before the first rect");
        assert_eq!(row(15), 0x44, "first rect content");
        assert_eq!(row(100), 0, "gap between the rects stays untouched");
        assert_eq!(row(299), 0, "row before the second rect");
        assert_eq!(row(305), 0x55, "second rect content");
        assert_eq!(row(511), 0, "untouched row after the last rect");
    }

    #[test]
    fn reconcile_pages_rejects_growth_without_a_full_page_upload_for_every_layer() {
        let mut uploads = AtlasUploads::new();
        for layer in [0u16, 2] {
            uploads.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 3),
                1,
                vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
                full_rects(),
            );
        }
        let error = uploads
            .reconcile_pages([spec(TEST_PAGE, 3), spec(1, 1)])
            .expect_err("缺层的扩层请求必须被拒绝");
        assert!(
            matches!(
                error,
                crate::error::EngineError::AtlasGrowthIncomplete {
                    kind: AtlasKind::Image,
                    layer: 1,
                    layers: 3,
                }
            ),
            "unexpected error: {error:?}"
        );

        let mut uploads = AtlasUploads::new();
        for layer in 0..3u16 {
            let rects = if layer == 1 { rows(2, 4) } else { full_rects() };
            let bytes = rects.byte_count(TEST_PAGE, 4);
            uploads.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 3),
                1,
                vec![0u8; bytes],
                rects,
            );
        }
        let error = uploads
            .reconcile_pages([spec(TEST_PAGE, 3), spec(1, 1)])
            .expect_err("非整页的扩层请求必须被拒绝");
        assert!(
            matches!(
                error,
                crate::error::EngineError::AtlasGrowthIncomplete {
                    kind: AtlasKind::Image,
                    layer: 1,
                    layers: 3,
                }
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn reconcile_pages_accepts_growth_when_every_layer_is_a_full_page() {
        let mut uploads = AtlasUploads::new();
        for layer in 0..3u16 {
            uploads.queue_upload(
                AtlasKind::Image,
                layer,
                spec(TEST_PAGE, 3),
                1,
                vec![0u8; (TEST_PAGE * TEST_PAGE * 4) as usize],
                full_rects(),
            );
        }
        assert_eq!(
            uploads
                .reconcile_pages([spec(TEST_PAGE, 3), spec(1, 1)])
                .expect("全层整页的扩层请求必须被接受"),
            vec![request(AtlasKind::Image, TEST_PAGE, 3)],
            "一次重建、层数取最大值"
        );
    }
}

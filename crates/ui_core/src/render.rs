pub(crate) mod atlas;
pub(crate) mod color;
pub(crate) mod colorspace;
pub(crate) mod compose;
pub(crate) mod compositor;
pub(crate) mod compositor_thread;
pub(crate) mod damage;
pub(crate) mod delta;
pub(crate) mod frame;
pub(crate) mod geometry_upload;
pub(crate) mod present_evidence;
pub(crate) mod retire;
#[cfg(test)]
mod shader_contract;
pub(crate) mod stats;
pub(crate) mod timer;
pub(crate) mod uniform;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;

pub(crate) use atlas::{
    Allocation, AtlasAllocator, AtlasArraySpec, DirtyRects, ImageAtlasPager, MIN_ATLAS_PAGE,
};
#[cfg(any(windows, target_os = "linux"))]
pub(crate) use atlas::{AtlasPageRequest, merge_page_request};
#[cfg(target_os = "linux")]
pub(crate) use linux as backend;
#[cfg(target_os = "macos")]
pub(crate) use macos as backend;
pub(crate) use stats::RenderStats;
#[cfg(windows)]
pub(crate) use windows as backend;

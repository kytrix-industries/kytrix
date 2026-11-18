//! 像素读回（L4）：真实 GPU 离屏渲染 + 读回，**带合成端滚动槽偏移表**。
//!
//! 为什么需要它：`TestApp::snapshot` 走 `Renderer::render_snapshot`，那条路径给
//! `FramePayload` 的滚动表恒为空（快照不滚），于是滚动后的内容会以**烘焙坐标**呈现 ——
//! 像素断言看不到位移。这里复用同一组生产类型（`GpuRenderer` / `FramePayload` /
//! `submit_snapshot` / `readback`），只是额外接受调用方给定的滚动槽表，与 golden 快照
//! 链路（`tests/snapshot.rs`）是同一台机器上的同一条 GPU 读回。
//!
//! **滚动表从哪来**：生产里 `DeltaBuilder` 把 paint pass 的槽位表填进 `FramePayload.scroll`；
//! 这里由调用方按公开 API 重建同一结构（`App::scroll_state` 的偏移 + 探针 bounds 换算出的
//! 物理视口）。生产侧「偏移真的进了 delta」由 `FrameRecord.stats.scroll_offset_frames` 单独
//! 断言（见 `super::gui_smoke`），两条断言合起来覆盖「滚轮 → 偏移 → 合成 → 呈现」整条链。
//!
//! 三平台同一份代码：类型路径统一走 `render::backend`（编译期选定恰好一份后端）。

use crate::{
    app::App,
    error::EngineError,
    math::{PhysicalRect, Point, Scale},
    paint::atlas::AtlasKind,
    render::{
        DirtyRects,
        backend::compositor::GpuRenderer,
        compositor::{FramePlan, Plan},
        delta::{ScrollDelta, ScrollTable},
        frame::FramePayload,
        timer::FrameTimings,
    },
    test_support::snapshot::{SnapshotError, SnapshotImage},
    window::{FrameIndex, Window},
};

/// 一帧的滚动槽表项：`(逻辑滚动位移, 物理视口)`；**下标即槽位**（下标 0 恒是哨兵）。
pub type ScrollSlot = (Point, PhysicalRect);

/// 渲染当前窗口的实例缓冲并读回（`scroll` 为空 = 不应用滚动偏移，等价于 `TestApp::snapshot`）。
///
/// `size` / `scale` 是场景的物理尺寸与 DPI（headless 窗口 = `size × scale` 的 covering）。
///
/// # Errors
/// [`EngineError::AppInit`] / [`EngineError::UnsupportedGpu`]：设备 / 管线 / 渲染 / 读回失败。
pub fn render_offscreen(
    app: &App,
    window: &Window,
    size: (u32, u32),
    scale: Scale,
    scroll: &[ScrollSlot],
) -> Result<SnapshotImage, EngineError> {
    let (offsets, clip) = scroll_payload(scroll, scale);
    let mut renderer = offscreen_gpu(size)?;
    // 字形页恒上传；图片页只在 App 侧有页时上传（与 `TestApp::snapshot` 同一口径）。
    let (glyph_page, glyph_pixels) = app.glyph_page_pixels();
    renderer.queue_atlas_upload(
        AtlasKind::Glyph,
        glyph_page.0,
        glyph_pixels,
        DirtyRects::full(),
    );
    if let Some((image_page, image_pixels)) = app.resource_page_pixels() {
        renderer.queue_atlas_upload(
            AtlasKind::Image,
            image_page.0,
            image_pixels.to_vec(),
            DirtyRects::full(),
        );
    }
    let buffer = window
        .instance_buffer()
        .ok_or_else(|| EngineError::AppInit {
            detail: smol_str::SmolStr::new_static(
                "pixel readback requires an inline (headless) window",
            ),
        })?;
    let payload = FramePayload::full(buffer, offsets, clip);
    let mut frame = FramePlan::new(Plan::Full, true);
    let mut timings = FrameTimings::new();
    renderer.submit_snapshot(FrameIndex::from_raw(1), &payload, &mut frame, &mut timings)?;
    let image = renderer.readback()?;
    Ok(SnapshotImage {
        width: image.width,
        height: image.height,
        rgba8: image.rgba8,
    })
}

/// 离屏渲染器：Windows 用 WARP（与 golden 基线同一设备类型），其余平台用真实 GPU 离屏。
fn offscreen_gpu(size: (u32, u32)) -> Result<GpuRenderer, EngineError> {
    #[cfg(windows)]
    {
        GpuRenderer::new_warp_offscreen(size)
    }
    #[cfg(not(windows))]
    {
        GpuRenderer::new_offscreen(size)
    }
}

/// 槽位表 → `(物理平移量, 屏幕裁剪区)`：直接走生产的 [`ScrollTable`]，不在这里复制符号约定。
fn scroll_payload(scroll: &[ScrollSlot], scale: Scale) -> (Vec<[f32; 2]>, Vec<[i32; 4]>) {
    let mut table = ScrollTable::new();
    let deltas: Vec<ScrollDelta> = scroll
        .iter()
        .enumerate()
        .map(|(slot, (offset, viewport))| ScrollDelta {
            slot: slot as u32,
            offset: *offset,
            viewport: *viewport,
        })
        .collect();
    table.apply(&deltas, scale);
    let mut offsets = Vec::with_capacity(scroll.len());
    let mut clip = Vec::with_capacity(scroll.len());
    table.to_payload(scale, &mut offsets);
    table.viewports_to_payload(&mut clip);
    (offsets, clip)
}

/// 读回一张图里的一个像素（RGBA8，预乘 sRGB）。
///
/// # Panics
/// 坐标越界（测试自己写错了断言位置）。
#[must_use]
pub fn pixel(image: &SnapshotImage, x: u32, y: u32) -> [u8; 4] {
    assert!(
        x < image.width && y < image.height,
        "pixel ({x},{y}) out of bounds ({}x{})",
        image.width,
        image.height
    );
    let offset = ((y as usize) * (image.width as usize) + (x as usize)) * 4;
    image.rgba8[offset..offset + 4]
        .try_into()
        .expect("4 bytes per pixel")
}

/// 两个颜色的最大单通道差（含 alpha；预乘空间）。
#[must_use]
pub fn channel_span(a: [u8; 4], b: [u8; 4]) -> u8 {
    a.iter()
        .zip(b.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0)
}

/// 矩形区域内与 `expected` 的最大单通道差（闭区间 `x0..=x1` / `y0..=y1`）。
///
/// 传全透明像素时 `expected` 的 alpha 必须一起对上：读回是预乘的，透明区恒 `[0, 0, 0, 0]`。
#[must_use]
pub fn region_max_span(
    image: &SnapshotImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    expected: [u8; 4],
) -> u8 {
    let mut span = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            span = span.max(channel_span(pixel(image, x, y), expected));
        }
    }
    span
}

/// 两张同尺寸图在矩形区域内的**最大单通道差**（「这块区域真的变了吗」的断言面）。
///
/// # Panics
/// 两张图尺寸不同（读回尺寸不该漂移）。
#[must_use]
pub fn region_diff(
    before: &SnapshotImage,
    after: &SnapshotImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
) -> u8 {
    assert_eq!(
        (before.width, before.height),
        (after.width, after.height),
        "readback sizes must not drift between frames"
    );
    let mut span = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            span = span.max(channel_span(pixel(before, x, y), pixel(after, x, y)));
        }
    }
    span
}

/// 矩形区域内满足谓词的像素数（文本覆盖率 / 高亮区域等断言用；闭区间）。
#[must_use]
pub fn count_pixels(
    image: &SnapshotImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    predicate: impl Fn([u8; 4]) -> bool,
) -> u32 {
    let mut count = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            if predicate(pixel(image, x, y)) {
                count += 1;
            }
        }
    }
    count
}

/// `SnapshotError` 的可读摘要（调用方拼断言消息用）。
#[must_use]
pub fn describe_error(error: &SnapshotError) -> String {
    match error {
        SnapshotError::NoRenderer => String::from("no renderer in this build"),
        SnapshotError::Gpu { detail } => format!("gpu: {detail}"),
    }
}

//! 渲染目标与平台表面：尺寸 / DPI 写入 / 渲染器 / 图集同步。

use super::{Window, WindowOptions};
use crate::{
    error::EngineError,
    math::{PhysicalRect, Rect, Scale, Size},
    paint::{atlas::AtlasKind, color::Color},
    render::backend::Renderer,
};

/// 图集行没赶上帧时，最多连续补几帧（窗口的共享槽长期不空时避免空转）。
const MAX_ATLAS_FOLLOWUPS: u8 = 3;

// 消费者：`impl Window::options` 的薄委托（外部调用点见该方法文档）。
/// 建窗参数快照。`background` 可在运行期被 [`Window::set_background_color`] 改写。
pub(crate) fn options(window: &Window) -> &WindowOptions {
    &window.options
}

// 消费者：`impl Window::set_background_color` 的薄委托（外部调用点见该方法文档）。
/// 改窗口背景色（同值 no-op）：写 `options.background`；有渲染器时把新颜色交给平台层。
///
/// Windows 上值变化会标全量重绘：组合交换链没有可用的 `SetBackgroundColor`，新颜色既作为
/// persist 的 clear color（linear），也重建独立的 **1×1 背景层**（`refresh_background`）。
/// GPU 线程在**空闲等待前**或下一次取帧录制前应用，因此不要求提交内容帧；UI 线程只写共享槽
/// 并唤醒（I1）。
///
/// macOS 上值变化同样标全量重绘：`CAMetalLayer.backgroundColor` 立刻在主线程写，persist 的
/// clear color 经共享槽交给 GPU 线程（同样在空闲或下一帧录制前应用）。
///
/// Linux 上值变化同样标全量重绘：clear color（linear）经共享槽交给 GPU 线程，在空闲或下一帧
/// 录制前写进持久目标，下一次 present 生效。
pub(crate) fn set_background_color(window: &mut Window, color: Color) {
    if window.options.background == color {
        return;
    }
    window.options.background = color;
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    {
        // 背景色不是绘制图元：没有 damage 的帧可能永远不被 present，必须显式标全量。
        window.full_damage_pending = true;
        if let Some(renderer) = window.renderer.as_mut() {
            renderer.set_background_color(color);
        }
    }
}

// 消费者：`impl Window::size` 的薄委托（外部调用点见该方法文档）。
/// 窗口逻辑尺寸（根布局的可用空间）。
pub(crate) fn size(window: &Window) -> Size {
    window.options.size
}

// 消费者：`impl Window::set_size` 的薄委托（外部调用点见该方法文档）。
/// 逻辑尺寸变化（消费者：P12 的 resize 事件）：写尺寸 + 标根 `LAYOUT` + 清根缓存。
///
/// **不调整 OS 窗口大小**：只改逻辑布局与 damage。平台层在真实 resize 之后用 `set_surface`
/// 写入真实渲染目标；平台已写过真值时本方法保留它，应用代码不要在活窗口上用本方法改
/// 客户区（F-186 / F-189）。只有平台尚未写入过目标（headless / 快照 / 首帧前）时，
/// `surface` 才跟随 `size × scale` 的 covering。
pub(crate) fn set_size(window: &mut Window, size: Size) {
    // 同值短路：DPI 路径当轮已按新 DPI 写回逻辑尺寸，随后那条 `WM_SIZE` 会以同值再调
    // 一次；不做早退就会多出一整帧布局 + 全量重绘（`set_surface` 一直是这么做的）。
    if window.size() == size {
        return;
    }
    window.options.size = size;
    window.full_damage_pending = true;
    let root = window.tree.root();
    // 与 `set_style` / `set_layout_override` 同一条失效路径（P11：它也标 `A11Y`）。
    window.tree.invalidate_layout(root);
    // 平台真值一旦写入就不再被 covering 覆盖（F-186）：否则应用在活窗口上调本方法会把
    // 渲染目标重置成 `size × scale` 的猜测值。
    if !window.surface_from_platform {
        window.surface = covering_surface(window.size(), window.scale);
    }
}

// 消费者：`impl Window::set_surface` 的薄委托（外部调用点见该方法文档）。
/// 平台层：写入真实渲染目标物理尺寸（Windows `WM_SIZE` / `WM_DPICHANGED`、macOS
/// `convertSizeToBacking` 之后、Linux `round(逻辑 × scale)` 的 Wayland buffer 尺寸）。
///
/// 值变化 ⇒ 标全量重绘（新暴露的像素没有对应 damage）；**不**标 layout、**不**动
/// `scale_epoch`：本方法只改渲染目标，不改布局；DPI 的布局变化由 `set_scale` / `set_size` 负责。
///
/// 调用一次之后 `surface` 归平台层所有：`set_size` / `set_scale` 不再用 covering 覆盖它
/// （F-186 / F-189）。同值调用也会把 `surface` 的所有权交给平台（先于 early-return）。
///
/// macOS 还必须在这里刷新图层属性（`contentsScale` / `drawableSize`）：这是主线程的写入口，
/// 且必须在「值没变就返回」**之前**执行 —— DPI 变化但 surface 恰好相同时也要更新
/// `contentsScale`。只变 scale 时也必须刷新。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) fn set_surface(window: &mut Window, surface: (u32, u32)) {
    let surface = (surface.0.max(1), surface.1.max(1));
    window.surface_from_platform = true;
    #[cfg(target_os = "macos")]
    if let Some(renderer) = window.renderer.as_mut() {
        renderer.set_layer_scale(surface, window.scale.factor());
    }
    if window.surface == surface {
        return;
    }
    window.surface = surface;
    window.full_damage_pending = true;
}

// 消费者：`impl Window::set_max_frame_rate` 的薄委托（外部调用点见该方法文档）。
/// macOS：换屏后把 display link 的偏好帧率更新到新屏幕的刷新率。
///
/// 旧实现只在建窗时读一次 `maximumFramesPerSecond`；窗口被拖到不同刷新率的屏幕上之后
/// 仍按旧值请求（ProMotion 120 Hz 屏上被限在 60 fps，反之在 60 Hz 屏上请求 120）。
/// 其他平台没有「每屏刷新率」概念，方法不定义（平台层按 cfg 调用）。
#[cfg(target_os = "macos")]
pub(crate) fn set_max_frame_rate(window: &mut Window, max_frame_rate: f32) {
    if let Some(renderer) = window.renderer.as_mut() {
        renderer.set_frame_rate(max_frame_rate);
    }
}

// 消费者：`impl Window::set_buffer_scale` 的薄委托（外部调用点见该方法文档）。
/// Linux：写入整数回退的 `wl_surface` buffer scale（fractional 路径恒 1）。
///
/// **不**标 damage / layout、**不**动 `scale_epoch`：它只是渲染目标的属性，随
/// [`FrameRequest`] 过线，由 GPU 线程在 present 前与 attach/commit 同一次生效（V-110）。
/// scale 本身的布局变化由 `set_scale` 负责；同值调用是 no-op，钳到 ≥1。
#[cfg(target_os = "linux")]
pub(crate) fn set_buffer_scale(window: &mut Window, scale: i32) {
    window.buffer_scale = scale.max(1);
}

// 消费者：`impl Window::has_renderer` 的薄委托（外部调用点见该方法文档）。
/// 是否已有 GPU 渲染器（`App::upload_atlas_pages` 的判据）。
pub(crate) fn has_renderer(window: &Window) -> bool {
    window.renderer.is_some()
}

// 消费者：`impl Window::glyph_page_received` 的薄委托（外部调用点见该方法文档）。
/// 该窗口的渲染器是否已收到完整字形图集页（测试诊断）。
#[cfg(test)]
pub(crate) fn glyph_page_received(window: &Window) -> bool {
    window.glyph_page_received.is_some()
}

// 消费者：`impl Window::image_page_received` 的薄委托（外部调用点见该方法文档）。
/// 该窗口的渲染器是否已收到完整图片图集页（测试诊断）。
#[cfg(test)]
pub(crate) fn image_page_received(window: &Window) -> bool {
    window.image_page_received.is_some()
}

// 消费者：`impl Window::atlas_page_matches` 的薄委托（外部调用点见该方法文档）。
/// 已收到的整页尺寸是否就是 `page`（否则这一帧必须整页重传）。
pub(crate) fn atlas_page_matches(window: &Window, kind: AtlasKind, page: u32) -> bool {
    match kind {
        AtlasKind::Glyph => window.glyph_page_received == Some(page),
        AtlasKind::Image => window.image_page_received == Some(page),
    }
}

// 消费者：`impl Window::mark_atlas_page_received` 的薄委托（外部调用点见该方法文档）。
/// 记账：该图集页已整页入队到本窗口的渲染器（记录页尺寸）。
pub(crate) fn mark_atlas_page_received(window: &mut Window, kind: AtlasKind, page: u32) {
    match kind {
        AtlasKind::Glyph => window.glyph_page_received = Some(page),
        AtlasKind::Image => window.image_page_received = Some(page),
    }
}

// 消费者：`impl Window::mark_atlas_page_stale` 的薄委托（外部调用点见该方法文档）。
/// 该图集在本窗口的整页记账作废：下一次必须整页重传（渲染器换代 / 图集清空）。
pub(crate) fn mark_atlas_page_stale(window: &mut Window, kind: AtlasKind) {
    match kind {
        AtlasKind::Glyph => window.glyph_page_received = None,
        AtlasKind::Image => window.image_page_received = None,
    }
}

// 消费者：`impl Window::atlas_rows` 的薄委托（外部调用点见该方法文档）。
/// 本窗口待上传的图集脏行（忙窗口攒着，空闲时补发）。
pub(crate) fn atlas_rows(window: &Window, kind: AtlasKind) -> &crate::render::DirtyRows {
    &window.atlas_rows[kind.index() as usize]
}

// 消费者：`impl Window::atlas_rows_mut` 的薄委托（外部调用点见该方法文档）。
/// 本窗口待上传的图集脏行（可变：并入本帧新写的行）。
pub(crate) fn atlas_rows_mut(
    window: &mut Window,
    kind: AtlasKind,
) -> &mut crate::render::DirtyRows {
    &mut window.atlas_rows[kind.index() as usize]
}

// 消费者：`impl Window::clear_atlas_rows` 的薄委托（外部调用点见该方法文档）。
/// 本窗口的待发行已入队（或已被整页上传覆盖）。
pub(crate) fn clear_atlas_rows(window: &mut Window, kind: AtlasKind) {
    window.atlas_rows[kind.index() as usize].clear();
}

// 消费者：`impl Window::atlas_rows_pending` 的薄委托（外部调用点见该方法文档）。
/// 本窗口还有没赶上帧的图集行：这一帧会以旧图集呈现，需要补一帧。
pub(crate) fn atlas_rows_pending(window: &Window) -> bool {
    window.atlas_rows.iter().any(|rows| !rows.is_empty())
}

// 消费者：`impl Window::note_atlas_deferral` 的薄委托（外部调用点见该方法文档）。
/// 记一次「图集行没赶上帧」；返回是否还应再补一帧（上限 [`MAX_ATLAS_FOLLOWUPS`]，
/// 槽长期不空时不能无限空转）。
pub(crate) fn note_atlas_deferral(window: &mut Window) -> bool {
    if window.atlas_deferrals >= MAX_ATLAS_FOLLOWUPS {
        return false;
    }
    window.atlas_deferrals += 1;
    true
}

// 消费者：`impl Window::reset_atlas_deferrals` 的薄委托（外部调用点见该方法文档）。
/// 图集行已经全部入队：补帧计数复位。
pub(crate) fn reset_atlas_deferrals(window: &mut Window) {
    window.atlas_deferrals = 0;
}

// 消费者：`impl Window::queue_atlas_upload` 的薄委托（外部调用点见该方法文档）。
/// 把一页图集像素交给渲染器；`false` = 没有渲染器或渲染器已关闭（**没有**入队）。
///
/// `pixels` 是 `rows` 顺序**紧密打包**的行（每行 `page * texel` 字节；整页上传时就是整页），
/// 因此增量上传只搬脏行（DEVIATIONS F-211）。消费者：`App::upload_atlas_pages`（逐窗口
/// 分发；图集页是进程级共享数据，纹理是每窗口一份）。
pub(crate) fn queue_atlas_upload(
    window: &mut Window,
    kind: AtlasKind,
    page: u32,
    pixels: &[u8],
    rows: crate::render::DirtyRows,
) -> bool {
    window
        .renderer
        .as_mut()
        .is_some_and(|renderer| renderer.queue_atlas_upload(kind, page, pixels, rows))
}

// 消费者：`impl Window::atlas_idle` 的薄委托（外部调用点见该方法文档）。
/// 该图集的图集上传槽是否空闲（UI 线程据此决定这一帧能否入队，DEVIATIONS F-211）。
pub(crate) fn atlas_idle(window: &Window, kind: AtlasKind) -> bool {
    window
        .renderer
        .as_ref()
        .is_none_or(|renderer| renderer.atlas_idle(kind))
}

// 消费者：`impl Window::set_renderer` 的薄委托（外部调用点见该方法文档）。
/// 挂上 GPU 渲染器（平台层在窗口创建之后调用；`Renderer` 已带 HWND swapchain）。
///
/// 换渲染器 = 换一套空纹理（`AtlasTextures`），所以两张图集页都要重新整页上传。
pub(crate) fn set_renderer(window: &mut Window, renderer: Renderer) {
    window.renderer = Some(renderer);
    window.glyph_page_received = None;
    window.image_page_received = None;
    window.atlas_rows = std::array::from_fn(|_| crate::render::DirtyRows::default());
}

// 消费者：`impl Window::poll_renderer` 的薄委托（外部调用点见该方法文档）。
/// 轮询 renderer 的反馈 / 错误（三平台窗口路径；空闲时也要跑）。
///
/// GPU 线程以错误退出后这里会持续返回同一个错误（粘性），平台泵在下一轮冒泡出去。
/// 反馈本身由渲染器消费（统计 / GPU 采样），这里只把 HUD 需要的发布时间延迟记到窗口上：
/// 反馈是最新者胜，窗口永远拿到最近一次完成帧的 `latency`。
pub(crate) fn poll_renderer(window: &mut Window) -> Result<(), EngineError> {
    let Some(renderer) = window.renderer.as_mut() else {
        return Ok(());
    };
    // 三平台窗口路径一致：`poll` 返回 `Result<Option<FrameFeedback>, EngineError>`，
    // 离屏路径恒为 `Ok(None)`。
    let feedback = renderer.poll()?;
    #[cfg(feature = "hud")]
    if let Some(feedback) = feedback {
        window.last_frame_latency = Some(feedback.latency);
    }
    #[cfg(not(feature = "hud"))]
    let _ = feedback;
    Ok(())
}

// 消费者：`impl Window::sync_pacing` 的薄委托（外部调用点见该方法文档）。
/// display link 的暂停/恢复（macOS 平台泵**每轮**调用一次，§5.3 / I3）。
///
/// 三平台里只有 macOS 需要它：Windows/Linux 的节奏完全由事件 / 合成器驱动，没有可暂停的源。
/// 离屏渲染器的 `sync_pacing` 是空操作。
#[cfg(target_os = "macos")]
pub(crate) fn sync_pacing(window: &mut Window, continuous: bool) {
    if let Some(renderer) = window.renderer.as_mut() {
        renderer.sync_pacing(continuous);
    }
}

// 消费者：`impl Window::enable_offscreen_renderer` 的薄委托（外部调用点见该方法文档）。
/// 启用**离屏** GPU 渲染（headless 行为不变：不调用就恒为 `None`）。
///
/// P09 的原生窗口层会在此之后 attach swapchain；本方法本身只把窗口的绘制目标变成
/// 持久 offscreen（快照 / 导出 / 无窗口验证），从不 present。
///
/// # Errors
/// `EngineError::UnsupportedGpu`（D8 基线不足）、`EngineError::AppInit`（设备 / 管线创建失败）、
/// `EngineError::ShaderLoad`（DXIL 产物缺失或被跳过）。
pub(crate) fn enable_offscreen_renderer(window: &mut Window) -> Result<(), EngineError> {
    if window.renderer.is_some() {
        return Ok(());
    }
    let surface = window.physical_surface();
    window.renderer = Some(Renderer::new_offscreen(surface)?);
    window.glyph_page_received = None;
    window.image_page_received = None;
    window.atlas_rows = std::array::from_fn(|_| crate::render::DirtyRows::default());
    Ok(())
}

// 消费者：`impl Window::physical_surface` 的薄委托（外部调用点见该方法文档）。
/// 窗口的物理像素尺寸（平台层写入的真实渲染目标；headless / 平台首次写入前是 covering）。
pub(crate) fn physical_surface(window: &Window) -> (u32, u32) {
    window.surface
}

// 消费者：`impl Window::presented_surface` 的薄委托（外部调用点见该方法文档）。
/// 最近一次**完成 present** 的内容尺寸（物理像素）。
///
/// Windows 的 DirectComposition 几何用它：交换链里实际有内容的区域就是这个尺寸，内容
/// visual 的 clip 按它设置（**不缩放**，恒 1:1）；客户区只决定背景层的缩放
/// （DEVIATIONS §36 F-205 / S-152）。
///
/// 没有渲染器（headless / 平台尚未 attach）时等于本帧目标尺寸：没有「已呈现内容」可言。
#[cfg(windows)]
pub(crate) fn presented_surface(window: &Window) -> (u32, u32) {
    match window.renderer.as_ref() {
        Some(renderer) => renderer.presented_surface(),
        None => window.surface,
    }
}

// 消费者：`impl Window::take_composition_pending` 的薄委托（外部调用点见该方法文档）。
/// 合成几何兜底标记：GPU 线程的两次唤醒都失败时置位，泵的 ②b′ 消费后强制重算几何。
#[cfg(windows)]
pub(crate) fn take_composition_pending(window: &Window) -> bool {
    window
        .renderer
        .as_ref()
        .is_some_and(Renderer::take_composition_pending)
}

// 消费者：`impl Window::frame_rect` 的薄委托（外部调用点见该方法文档）。
/// 本帧的物理矩形（`paint_frame` / `commit_frame` 的唯一来源）。
///
/// 三平台一致：平台层通过 `set_surface` 写入的真实渲染目标 —— Windows 真实客户区
/// （至少 1 px）、macOS 真实 backing（`convertSizeToBacking`，文字按真实像素 1:1）、
/// Linux Wayland buffer 的 `round(逻辑 × scale)`（文字按 buffer 像素 1:1）。
/// headless / 平台首次写入之前是 `size × scale` 的 covering。
pub(crate) fn frame_rect(window: &Window) -> PhysicalRect {
    PhysicalRect::new(0, 0, window.surface.0 as i32, window.surface.1 as i32)
}

// 消费者：`Window::new`、`set_size` / `set_scale`（平台真值写入前的 covering）。
/// `size × scale` 的 covering 物理尺寸（向外 floor/ceil，至少 1 px）。
///
/// 只在平台层首次调用 [`Window::set_surface`] 之前使用（headless / 快照 / 首帧前）；平台真值
/// 一旦写入，`set_size` / `set_scale` 就保留它，不再用 covering 覆盖（F-186 / F-189 / V-122）。
pub(crate) fn covering_surface(size: Size, scale: Scale) -> (u32, u32) {
    let rect = Rect::new(0.0, 0.0, size.width, size.height).to_physical_covering(scale);
    (rect.width().max(1) as u32, rect.height().max(1) as u32)
}

//! 文本 pass（shape / layout）、字体栈、诊断，以及字形 / 图片图集页上传。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use std::rc::Rc;

use smallvec::SmallVec;

use super::App;
use crate::{
    error::EngineError,
    math::Size,
    text::{FontStack, FontStackStatus, TextDiagnostic, TextLayoutReport, TextPassReport},
    window::WindowId,
};

/// 把两张图集页分发给所有窗口的渲染器（P13 §8.2；评审阻塞 ②）。
///
/// 图集页是**进程级**共享数据（`AtlasAllocator` / `GlyphAtlas` 一份），纹理是**每窗口一份**
/// （`AtlasTextures`）。`frame_all` 逐窗提交，若第一次 `take` 就把脏行消费给当前窗口，
/// 其余窗口的纹理永远收不到内容。因此脏行由本方法统一消费一次，然后**逐窗口**分发：
///
/// 1. 还没收到过整页的窗口（新建窗口 / 换过渲染器 / 页尺寸变化）拿整页，其余按脏行增量；
/// 2. 共享槽忙的窗口不等待：本帧的脏行并进它自己的待发行，槽空闲后补发。任何一个窗口
///    不出帧都不会拖住其它窗口（DEVIATIONS F-211）；
/// 3. 窗口只有在**真正入队成功**之后才记账「已收到整页」。
///
/// 调用点：`App::frame`（每帧，paint 之前）。`TestApp::snapshot` 走自己的临时渲染器。
/// 消费者：`App::upload_atlas_pages`。
pub(crate) fn upload_atlas_pages(app: &mut App) {
    app.upload_glyph_page();
    app.upload_image_page();
}

/// 字形页分发：页级变化（尺寸 / 清空）走整页，其余按脏矩形（AOI）增量。
/// 消费者：`App::upload_glyph_page`。
pub(crate) fn upload_glyph_page(app: &mut App) {
    use crate::paint::atlas::AtlasKind;

    let page = app.atlas.page();
    debug_assert_eq!(page.0, page.1, "glyph atlas page must stay square");
    let extent = page.0;
    let len = extent as usize * extent as usize;

    // 图集被清空过（DPI 换代 / 容量逃逸重排）：旧代际的 tile 全部失效，CPU 页与每个
    // 窗口的纹理都要整页重建（padding 里可能残留旧字形的覆盖度）。
    let cleared = app.glyph_atlas.take_cleared();
    if cleared {
        app.pending_dirty.clear();
        app.glyph_page_extent = None;
        for window in app.windows.values_mut() {
            window.clear_atlas_rects(AtlasKind::Glyph);
            window.mark_atlas_page_stale(AtlasKind::Glyph);
        }
    }
    // 页尺寸变化：整页缓冲按新尺寸重建并**清零**。旧页的行宽不同，直接沿用会把旧字节
    // 解释成错位内容，1 纹素 padding 的线性采样会把它读进 tile 边缘。
    let page_changed = app.glyph_page_extent != Some(extent);
    if page_changed {
        app.pending_dirty.clear();
        app.glyph_page.clear();
        app.glyph_page.resize(len, 0);
        app.glyph_page_extent = Some(extent);
    }
    // 横跨页变化的行不再有意义（整页会覆盖全部内容），但整页必须由当前记录重建。
    let refresh = cleared || page_changed;

    let dirty = std::mem::take(&mut app.pending_dirty);
    let has_rects = !dirty.is_empty();
    let any_missing = app.windows.values().any(|window| {
        window.has_renderer() && !window.atlas_page_matches(AtlasKind::Glyph, extent)
    });
    // 只有「缺页且空闲」的窗口会在本帧拿整页；忙的缺页窗口等它空闲那一帧再重建整页
    // （否则一个长期忙的窗口会让 `write_page` 每帧重写整页）。
    let any_stale_idle = app.windows.values().any(|window| {
        window.has_renderer()
            && !window.atlas_page_matches(AtlasKind::Glyph, extent)
            && window.atlas_idle(AtlasKind::Glyph)
    });
    let any_queued = app
        .windows
        .values()
        .any(|window| window.has_renderer() && !window.atlas_rects(AtlasKind::Glyph).is_empty());
    if !has_rects && !any_missing && !any_queued {
        return;
    }
    // 整页重传（新窗口 / 页尺寸变化 / 图集清空）必须按**当前**记录重建整页；
    // 纯增量帧只写脏矩形覆盖到的字形（代价随脏矩形数而不是字形总数增长）。
    if refresh || any_stale_idle {
        app.glyph_atlas.write_page(&mut app.glyph_page, extent);
    } else if has_rects {
        app.glyph_atlas
            .write_rows(&mut app.glyph_page, extent, &dirty);
    }
    let full_rects = crate::render::DirtyRects::full();

    for window in app.windows.values_mut() {
        if !window.has_renderer() {
            continue;
        }
        if !window.atlas_page_matches(AtlasKind::Glyph, extent) {
            // 整页是唯一正确的来源：页尺寸变了 / 图集清空过 / 新渲染器。
            if !window.atlas_idle(AtlasKind::Glyph) {
                continue;
            }
            let uploaded = window.queue_atlas_upload(
                AtlasKind::Glyph,
                extent,
                &app.glyph_page,
                full_rects.clone(),
            );
            if uploaded {
                window.mark_atlas_page_received(AtlasKind::Glyph, extent);
                window.clear_atlas_rects(AtlasKind::Glyph);
            }
            continue;
        }
        if has_rects {
            window.atlas_rects_mut(AtlasKind::Glyph).merge_from(&dirty);
        }
        if window.atlas_rects(AtlasKind::Glyph).is_empty() || !window.atlas_idle(AtlasKind::Glyph) {
            continue;
        }
        // 整页脏（矩形数超上限折叠）：直接用整页缓冲，省掉「打包再复制」的一整页拷贝。
        if window.atlas_rects(AtlasKind::Glyph).is_full() {
            if window.queue_atlas_upload(
                AtlasKind::Glyph,
                extent,
                &app.glyph_page,
                full_rects.clone(),
            ) {
                window.clear_atlas_rects(AtlasKind::Glyph);
            }
            continue;
        }
        let mut packed = std::mem::take(&mut app.packed);
        crate::render::atlas::pack_rects(
            &app.glyph_page,
            extent,
            1,
            window.atlas_rects(AtlasKind::Glyph),
            &mut packed,
        );
        let rects = window.atlas_rects(AtlasKind::Glyph).clone();
        if window.queue_atlas_upload(AtlasKind::Glyph, extent, &packed, rects) {
            window.clear_atlas_rects(AtlasKind::Glyph);
        }
        app.packed = packed;
    }
}

/// 图片页分发：懒分配（没插过图片就没有页）+ 按脏矩形（AOI）增量（DEVIATIONS F-211 / R06-3）。
/// 消费者：`App::upload_image_page`。
pub(crate) fn upload_image_page(app: &mut App) {
    use crate::paint::atlas::AtlasKind;

    // 先取脏矩形再借页（`take_dirty_rects` 需要 `&mut`）：没有页时脏矩形必然是空的。
    let dirty = app.resources.take_dirty_rects();
    let Some((page, pixels)) = app.resources.page_pixels() else {
        return;
    };
    debug_assert_eq!(page.0, page.1, "image atlas page must stay square");
    let extent = page.0;
    let has_rects = !dirty.is_empty();
    let any_missing = app.windows.values().any(|window| {
        window.has_renderer() && !window.atlas_page_matches(AtlasKind::Image, extent)
    });
    let any_queued = app
        .windows
        .values()
        .any(|window| window.has_renderer() && !window.atlas_rects(AtlasKind::Image).is_empty());
    if !has_rects && !any_missing && !any_queued {
        return;
    }
    let full_rects = crate::render::DirtyRects::full();

    for window in app.windows.values_mut() {
        if !window.has_renderer() {
            continue;
        }
        if !window.atlas_page_matches(AtlasKind::Image, extent) {
            if !window.atlas_idle(AtlasKind::Image) {
                continue;
            }
            let uploaded =
                window.queue_atlas_upload(AtlasKind::Image, extent, pixels, full_rects.clone());
            if uploaded {
                window.mark_atlas_page_received(AtlasKind::Image, extent);
                window.clear_atlas_rects(AtlasKind::Image);
            }
            continue;
        }
        if has_rects {
            window.atlas_rects_mut(AtlasKind::Image).merge_from(&dirty);
        }
        if window.atlas_rects(AtlasKind::Image).is_empty() || !window.atlas_idle(AtlasKind::Image) {
            continue;
        }
        // 整页脏（矩形数超上限折叠）：直接用整页缓冲，省掉「打包再复制」的一整页拷贝。
        if window.atlas_rects(AtlasKind::Image).is_full() {
            if window.queue_atlas_upload(AtlasKind::Image, extent, pixels, full_rects.clone()) {
                window.clear_atlas_rects(AtlasKind::Image);
            }
            continue;
        }
        let mut packed = std::mem::take(&mut app.packed);
        crate::render::atlas::pack_rects(
            pixels,
            extent,
            4,
            window.atlas_rects(AtlasKind::Image),
            &mut packed,
        );
        let rects = window.atlas_rects(AtlasKind::Image).clone();
        if window.queue_atlas_upload(AtlasKind::Image, extent, &packed, rects) {
            window.clear_atlas_rects(AtlasKind::Image);
        }
        app.packed = packed;
    }
}

/// 字形页的整页像素（`R8`；快照路径用；只在 `test-support` 构建里存在）。
/// 消费者：`App::glyph_page_pixels`。
#[cfg(feature = "test-support")]
pub(crate) fn glyph_page_pixels(app: &App) -> ((u32, u32), Vec<u8>) {
    let page = app.atlas.page();
    let mut pixels = vec![0u8; (page.0 as usize) * (page.1 as usize)];
    app.glyph_atlas.write_page(&mut pixels, page.0);
    (page, pixels)
}

/// 图片页的当前内容（BGRA8 预乘 sRGB；`TestApp::snapshot` 的上传源）；没插过图片时 `None`。
/// 消费者：`App::resource_page_pixels`。
#[cfg(feature = "test-support")]
pub(crate) fn resource_page_pixels(app: &App) -> Option<((u32, u32), &[u8])> {
    app.resources.page_pixels()
}

// ── P05：文本 pass / 字体栈 / 诊断 ────────────────────────────────────────

/// 文本 pass：形状（缓存）→ 光栅 → 图集 → `ShapedLine`，尺寸变化时置 `LAYOUT`。
///
/// **必须在 `refresh_styles` 之后、`Window::compute_layout` 之前调用**
/// （帧序：styles → text → layout）。`NodeBuilder::text` 不设 measure，因此没跑本 pass 的
/// 文本节点 measure 为 `0×0`。
///
/// 返回 `None` = 窗口不存在；`Some(Err(..))` = `TextShape` / `AtlasExhausted`（调用方丢弃本帧）。
/// 消费者：`App::refresh_text`。
pub(crate) fn refresh_text(
    app: &mut App,
    window: WindowId,
) -> Option<Result<TextPassReport, EngineError>> {
    let App {
        text,
        raster,
        text_dirty_scratch,
        windows,
        scale_epochs,
        pending_dirty,
        ..
    } = app;
    let window_id = window;
    let scale_epoch = windows.get(&window_id)?.scale_epoch();
    // R12-4：DPI 变化**不再**清形状缓存（布局在逻辑坐标里，与 scale 无关），也**不再**清字形
    // 图集（新 ppem 的 tile 直接加入，旧 ppem 的由逐 tile LRU 淘汰，见 R12-1）。只需让该窗口
    // 的 `ShapedLine` 重排一次：tile 落点/尺寸是「物理取整后除回逻辑」的量，scale 变了要重算。
    if scale_epochs.get(&window_id).copied().unwrap_or(0) != scale_epoch {
        scale_epochs.insert(window_id, scale_epoch);
        if let Some(target) = windows.get_mut(&window_id) {
            target.tree_mut().invalidate_text_lines();
        }
    }
    raster.set_font_epoch(text.font_epoch());

    let window = windows.get_mut(&window_id)?;
    let tree = window.tree_mut();
    let mut report = TextPassReport::default();
    let known = taffy::Size {
        width: None,
        height: None,
    };
    let available = taffy::Size {
        width: taffy::AvailableSpace::MaxContent,
        height: taffy::AvailableSpace::MaxContent,
    };

    let mut index = 0usize;
    // P1-2.3：只遍历**文本脏集合**，不再每帧扫整棵树。
    //
    // 旧实现每帧 `for node in tree.order()`，对每个文本节点 `text_content(node).cloned()`
    // 再算全文哈希 —— 即使这一帧没有任何文本变化。32 个实时标签的实测代价约 6.8 ms/帧
    // （shape 3.2 + layout/raster 3.6），已接近 60 Hz 预算的一半。
    //
    // 集合来源：`set_text_content`（`bind_text` 求值 / 静态挂载）与 `mark_all_text_dirty`
    // （字体栈、系统字体、图集换代）。
    tree.take_text_dirty(text_dirty_scratch);
    let dirty: &[crate::node::id::NodeId] = text_dirty_scratch;
    while index < dirty.len() {
        let node = dirty[index];
        index += 1;
        let Some(content) = tree.text_content(node).cloned() else {
            continue;
        };
        let (shaped, hit) = text.shape_cached(&content);
        if hit {
            report.reused += 1;
        } else {
            report.shaped += 1;
        }
        let key = crate::text::cache::ShapeKey::new(&content, text.font_stack_version());
        if let Some(slot) = tree.text_slot(node)
            && slot.key() == key
            && slot.scale_epoch() == scale_epoch
            && Rc::ptr_eq(slot.shaped(), &shaped)
        {
            // B4：快照未变 ⇒ 不重建、不清缓存、不重挂（空闲帧只走这条路）。
            report.skipped += 1;
            continue;
        }

        let measured = shaped.measure(known, available);
        let previous = tree.text_measured_size(node).unwrap_or(Size::ZERO);
        // `LayoutOverride` 节点的尺寸由应用固定，文本 intrinsic size 变化不可能影响布局；
        // 不置 LAYOUT，避免 HUD 每 500 ms 触发一次全窗口重排。`layout_text` 仍会按新
        // ShapedText 重排/重建 ShapedLine（`set_text_shaped` 已清 line/width）。
        let fixed_layout = tree
            .get(node)
            .is_ok_and(|target| target.layout_override().is_some());
        let size_changed = !fixed_layout
            && (previous.width.to_bits() != measured.width.to_bits()
                || previous.height.to_bits() != measured.height.to_bits());
        tree.set_text_shaped(
            node,
            Rc::clone(&shaped),
            key,
            scale_epoch,
            measured,
            size_changed,
        );
    }
    // 数字快速路径的命中率（R12 的核心性能指标）：分子 = 走快速路径的塑形数，
    // 分母 = 进入快速路径判定的塑形数。
    let fast = text.take_fast_path_stats();
    report.fast_path_shaped = fast.hits;
    report.fast_path_queries = fast.queries;
    // 光栅/图集统计**不在这里**：光栅发生在同帧的 `layout_text`（第二阶段），而本报告在
    // 第一阶段产出。`GlyphAtlas::take_stats` 只能取一次，第二阶段才是唯一正确的消费点
    // （此前这里取到的永远是 0，复审已确认）。见 `TextLayoutReport` 的同名字段。
    report.dirty_spans = pending_dirty.rects().len();
    Some(Ok(report))
}

/// 文本的**第二阶段**：`compute_layout` 之后，按每个文本节点的**内容宽**重排、光栅、装箱。
///
/// 帧序（B1）：`refresh_styles` → `refresh_text` → `Window::compute_layout` → `layout_text` → paint。
/// 与 `refresh_text` 的分工：前者只 shape + 量固有尺寸（给 measure 用），后者拿 taffy 算出的
/// **内容宽**做最终断行（就地重排，零 clone）并产出 `ShapedLine`。
///
/// **只遍历重排脏集合**（R12-14）：`set_text_shaped` / 内容宽变化 / 图集淘汰 / 全量失效
/// 才入队，因此「没有文本变化的帧」这里是 O(1)。
///
/// 图集逐 tile 淘汰（R12-1）可能让**本次 pass 早先**生成的 `ShapedLine` 里的 tile 被复用：
/// 一旦淘汰发生，整 pass 重跑（最多 [`MAX_LAYOUT_TEXT_PASSES`] 轮），并把其它窗口的文本
/// `ShapedLine` 也标记为待重排（图集是进程级共享的）。**任何情况下都不返回 `TextShape`**。
/// 极端容量不足撞到轮数上限时，本窗口这一帧的行被清空、但节点留在重排队列里 —— 下一帧重建，
/// 不是永久丢文本。
///
/// # 容量不足（软失败）的闭环（R12-1 / P1-①）
///
/// `GlyphAtlas::ensure` 在「整页都是本 pass 工作集」时返回 `Ok(None)` 且**不写** `missing`
/// 负缓存。这类字形在本 pass 里按下面四步处理，任何一步都不丢帧：
///
/// 1. **可观测**：`GlyphAtlasStats::skipped` 累加进 [`TextLayoutReport::skipped_glyphs`]；
/// 2. **有诊断**：该节点本轮压**一条** [`TextDiagnostic::AtlasFull`]（不按字形刷屏；
///    `TextContext` 的诊断是 64 条环形缓冲，不会无限增长）；
/// 3. **能恢复**：该节点重新入下一帧的重排队列（`mark_text_layout_dirty`），并且本帧发布的
///    「少画几个字形」的 `ShapedLine` 带 [`PENDING_ATLAS_RETRY_GENERATION`] 标记 —— 下一帧的
///    复用判据必然不成立，会真正重光栅重试；
/// 4. **失败可见**：连续 `text::atlas::ATLAS_FULL_ESCALATION_FRAMES`（3）个 pass 仍有软失败
///    ⇒ 返回 [`EngineError::AtlasExhausted`]（`AtlasKind::Glyph`），计数随即清零以免每帧都报错。
///
/// 消费者：`App::layout_text`。
pub(crate) fn layout_text(
    app: &mut App,
    window: WindowId,
) -> Option<Result<TextLayoutReport, EngineError>> {
    let App {
        text,
        raster,
        atlas,
        glyph_atlas,
        windows,
        pending_dirty,
        text_layout_scratch,
        ..
    } = app;
    let window_id = window;
    let scale = windows.get(&window_id)?.scale().factor();
    let mut report = TextLayoutReport::default();
    let mut any_eviction = false;
    let mut scratch = Vec::new();
    // LRU 的 pass 边界：本 pass 用到的 tile 记 `last_used_pass = pass`，且在本 pass 内不可淘汰。
    glyph_atlas.begin_pass();
    {
        let window = windows.get_mut(&window_id)?;
        window
            .tree_mut()
            .take_text_layout_dirty(text_layout_scratch);
    }
    if text_layout_scratch.is_empty() {
        // 本 pass 没有任何重排 ⇒ 没有字形被跳过（`skipped == 0`）：清零连续软失败计数。
        // `note_soft_fail(0)` 恒返回 `Ok`，这里不可能产生错误。
        let _ = glyph_atlas.note_soft_fail(0);
        return Some(Ok(report));
    }

    let mut round = 0usize;
    loop {
        report.rasterized = 0;
        report.tiles_reused = 0;
        report.atlas_rebuilds = 0;
        report.atlas_evictions = 0;
        report.skipped_glyphs = 0;
        let generation = atlas.generation();
        let mut laid = 0usize;
        let mut reused = 0usize;
        // 先取走上一轮的淘汰计数，才能只统计本轮新增的淘汰。
        let _previous_evictions = glyph_atlas.take_evicted();
        let window = windows.get_mut(&window_id)?;
        let tree = window.tree_mut();
        let mut index = 0usize;
        while index < text_layout_scratch.len() {
            let node = text_layout_scratch[index];
            index += 1;
            let Some(shaped) = tree.text_slot(node).map(|slot| Rc::clone(slot.shaped())) else {
                continue;
            };
            let width = {
                let width = tree.text_content_width(node);
                if width > 0.0 {
                    width
                } else {
                    shaped.max_content_size().width
                }
            };
            let current = tree
                .text_slot(node)
                .map(|slot| (slot.laid_width_bits(), slot.laid_generation()));
            if current == Some((width.to_bits(), generation)) && tree.text_line(node).is_some() {
                reused += 1;
                continue;
            }
            let Some(content) = tree.text_content(node) else {
                continue;
            };
            let color = content.style.color;
            let alignment = content.style.alignment;
            let ellipsis = text.ellipsis_for(&content.style);
            let mut ctx = crate::text::shape::RasterCtx {
                raster,
                glyph_atlas,
                atlas,
                generation,
                scale,
            };
            let (line, mut diagnostics, stats) =
                match shaped.rasterize(width, alignment, color, &mut ctx, ellipsis.as_deref()) {
                    Ok(value) => value,
                    Err(error) => return Some(Err(error)),
                };
            // 图集统计的唯一消费点（`take_stats` 在 `rasterize` 内部完成）：按节点累加。
            report.rasterized += stats.rasters;
            report.tiles_reused += stats.hits;
            report.atlas_rebuilds += stats.rebuilds;
            report.atlas_evictions += stats.evictions;
            report.skipped_glyphs += stats.skipped;
            pending_dirty.merge(&mut atlas.take_dirty_rects());
            // 软失败（整页都是本 pass 工作集）的闭环：可观测 + 诊断 + 下一帧重试。
            // 每节点每轮**只**压一条诊断（不按字形刷屏），并重新入下一帧的重排队列。
            let soft_fail = stats.skipped > 0;
            if soft_fail {
                diagnostics.push(TextDiagnostic::AtlasFull {
                    skipped: stats.skipped,
                });
                tree.mark_text_layout_dirty(node);
            }
            text.push_diagnostics(diagnostics);
            // 软失败的行不是本代际的完整结果：槽里的代际写「待重排」哨兵，下一帧的复用判据
            // 必然不成立 ⇒ 真正重光栅重试（本帧照常发布这份少画了几个字形的 `ShapedLine`）。
            let laid_generation = if soft_fail {
                PENDING_ATLAS_RETRY_GENERATION
            } else {
                generation
            };
            tree.set_text_line(node, Rc::new(line), width, laid_generation);
            laid += 1;
        }
        let evicted = glyph_atlas.take_evicted();
        let generation_changed = atlas.generation() != generation;
        report.laid = laid;
        report.reused = reused;
        report.reruns = round;
        if evicted == 0 && !generation_changed {
            any_eviction |= report.atlas_evictions > 0;
            break;
        }
        // 有 tile 被淘汰 / 图集换代：本 pass 早先的 `ShapedLine` 可能引用已失效的矩形。
        // 把全部文本重新入队后重跑（有界：淘汰只发生在容量不足时，第二轮会优先淘汰
        // 不在本帧使用的 tile，工作集放得下就收敛）。
        any_eviction = true;
        tree.invalidate_text_lines();
        round += 1;
        if round >= MAX_LAYOUT_TEXT_PASSES {
            // 兜底：不再重跑本帧。此时行刚被清空、全部文本节点**已入队**（`invalidate_text_lines`），
            // 下一帧的 `layout_text` 会把它们重建 —— 不要先 `take_text_layout_dirty` 把队列换成
            // 下一轮的列表（那张列表下一帧会被清空重填，节点就会停在「无行且不在队列」的永久丢文本状态）。
            break;
        }
        scratch.clear();
        tree.take_text_layout_dirty(&mut scratch);
        std::mem::swap(&mut scratch, text_layout_scratch);
    }

    // 图集是进程级共享的：本窗口的淘汰会让其它窗口的 `ShapedLine` 引用已失效的矩形。
    // 让它们在自己的下一帧重排（各自的 `layout_text` 都在 paint 之前，所以不会画旧 UV）。
    if any_eviction {
        for (id, other) in windows.iter_mut() {
            if *id == window_id {
                continue;
            }
            other.tree_mut().invalidate_text_lines();
        }
    }
    // 可见失败兜底：连续 `ATLAS_FULL_ESCALATION_FRAMES` 个 pass 都以软失败收尾 ⇒ 上报
    // `AtlasExhausted`（调用方丢弃这一帧）。计数在同一次调用里清零，不是每帧都报错；
    // 一次 `skipped == 0` 的 pass 会把它清零（见 `GlyphAtlas::note_soft_fail`）。
    if let Err(error) = glyph_atlas.note_soft_fail(report.skipped_glyphs) {
        return Some(Err(error));
    }
    Some(Ok(report))
}

/// 单帧内 `layout_text` 的最大重跑轮数（图集淘汰后整 pass 重来）。
pub(crate) const MAX_LAYOUT_TEXT_PASSES: usize = 4;

/// 软失败 `ShapedLine` 的「待重排」代际标记（R12-1 / P1-①）。
///
/// `layout_text` 的复用判据是「槽里记的 `(内容宽, 图集代际)` 与本次一致，且已有
/// `ShapedLine`」。因图集放不下而少画了字形的行**不是**本代际的完整结果，必须下一帧重排
/// 重试：把槽里记的代际写成这个哨兵值，复用判据必然不成立。
///
/// 真实代际由 `AtlasAllocator` 换代递增（`wrapping_add(1)`，从 1 起；每次换代都是整页重建），
/// 需要 2^32−1 次整页重建才会撞上这个值 —— 实际不可能发生；即便发生，也只是少一次重试
/// （那一行的缺口会在下一次自然失效时补上）。
const PENDING_ATLAS_RETRY_GENERATION: u32 = u32::MAX;

/// 当前字体栈。
/// 消费者：`App::font_stack`。
#[must_use]
pub(crate) fn font_stack(app: &App) -> &FontStack {
    app.text.font_stack()
}

/// 当前字体栈版本（形状缓存键的一部分）。
/// 消费者：`App::font_stack_version`。
#[must_use]
pub(crate) fn font_stack_version(app: &App) -> u32 {
    app.text.font_stack_version()
}

/// 数字快速路径开关（回退验证用；R12 的 `DigitFastPath`）。
///
/// 关闭后所有文本都走 parley，形状缓存被清空（两种模式的快照不混用）。
/// 消费者：`App::set_digit_fast_path`。
pub(crate) fn set_digit_fast_path(app: &mut App, enabled: bool) {
    app.text.set_digit_fast_path(enabled);
}

/// 数字快速路径是否开启。
/// 消费者：`App::digit_fast_path_enabled`。
#[must_use]
pub(crate) fn digit_fast_path_enabled(app: &App) -> bool {
    app.text.digit_fast_path_enabled()
}

/// 文本诊断（tofu / 行内盒边界 / 超宽 / 合成样式）。
/// 消费者：`App::text_diagnostics`。
#[must_use]
pub(crate) fn text_diagnostics(app: &App) -> &[TextDiagnostic] {
    app.text.diagnostics()
}

/// 字体栈的健康状态（N4：测试与诊断的**唯一**跳过判据）。
/// 消费者：`App::font_stack_status`。
pub(crate) fn font_stack_status(app: &mut App) -> FontStackStatus {
    app.text.font_stack_status()
}

/// 换字体栈：版本 +1、字体代际 +1、清形状缓存与字形图集、所有窗口的文本节点
/// `LAYOUT | PAINT`。返回新版本号。
/// 消费者：`App::set_font_stack`。
pub(crate) fn set_font_stack(app: &mut App, stack: FontStack) -> u32 {
    let version = app.text.set_font_stack(stack);
    app.glyph_atlas.clear();
    app.atlas.clear();
    app.raster.set_font_epoch(app.text.font_epoch());
    app.invalidate_all_text();
    version
}

/// 系统字体安装/卸载/语言切换后调用：重载 + 与 [`App::set_font_stack`] 相同的失效。
/// 消费者：`App::reload_system_fonts`。
pub(crate) fn reload_system_fonts(app: &mut App) -> u32 {
    let version = app.text.reload_system_fonts();
    app.glyph_atlas.clear();
    app.atlas.clear();
    app.raster.set_font_epoch(app.text.font_epoch());
    app.invalidate_all_text();
    version
}

/// 所有窗口的文本节点标 `LAYOUT | PAINT` 并清 `node..root` 缓存。
/// 图集代际变化时让**所有窗口**的文本节点重绘（P1-2.6）。
///
/// 图集换代（DPI 变化 / 字形扩容逃逸）会让页内所有 tile 重排，而每个文本 lane 里存的是
/// 生成时的 `AtlasTile`（旧 UV）。`set_text_line` 不标 `PAINT`，paint 的 lane 复用只看
/// `REUSABLE`/`clip_key`，于是旧 UV 被继续使用 —— 表现为错字/花字，且要等一次无关的
/// `PAINT` 才可能恢复。
///
/// 只标 `PAINT` 不标 `LAYOUT`：换代不改变字形宽度与断行，重排是纯损失。
///
/// 对照 Skia：glyph atlas / strike cache 换代时会让所有引用旧 strike 的绘制数据失效。
/// 消费者：`App::repaint_text_if_atlas_changed`。
pub(crate) fn repaint_text_if_atlas_changed(app: &mut App) {
    let generation = app.atlas.generation();
    if generation == app.atlas_generation_seen {
        return;
    }
    app.atlas_generation_seen = generation;
    for window in app.windows.values_mut() {
        window.tree_mut().mark_all_text_paint();
    }
}

/// 消费者：`App::invalidate_all_text`。
pub(crate) fn invalidate_all_text(app: &mut App) {
    let ids: SmallVec<[WindowId; 4]> = app.windows.keys().copied().collect();
    for id in ids {
        let Some(window) = app.windows.get_mut(&id) else {
            continue;
        };
        let tree = window.tree_mut();
        // 冷路径（换字体栈 / 系统字体变化）：一次 DFS 收集文本节点，不保留全序数组。
        let mut nodes: Vec<crate::node::id::NodeId> = Vec::new();
        tree.for_each_dfs(|node| {
            if tree.text_content(node).is_some() {
                nodes.push(node);
            }
        });
        for node in nodes {
            tree.invalidate_text_layout(node);
        }
        // P1-2.3：字体栈 / 系统字体变化后，脏集合必须包含**全部**文本节点 ——
        // `refresh_text` 现在只遍历脏集合，漏标就等于永久不重排。
        tree.mark_all_text_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::MAX_LAYOUT_TEXT_PASSES;
    use crate::{
        app::App,
        error::EngineError,
        node::{builder::NodeBuilder, id::NodeId},
        paint::atlas::AtlasKind,
        render::AtlasAllocator,
        text::{TextDiagnostic, TextStyle, atlas::ATLAS_FULL_ESCALATION_FRAMES},
        window::{WindowId, WindowOptions},
    };

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let app = App::new(rt.handle().clone());
        (rt, app)
    }

    fn text_style(width: Option<f32>) -> crate::paint::style::Style {
        let mut style = crate::paint::style::Style::default();
        style.inner.align_self = Some(taffy::style::AlignSelf::START);
        if let Some(width) = width {
            style.inner.size.width = taffy::style::Dimension::length(width);
        }
        style
    }

    fn text_app(
        text: &str,
        width: Option<f32>,
    ) -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
        let (rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        let node = NodeBuilder::text(text, TextStyle::new(16.0))
            .style(text_style(width))
            .build(w.tree_mut(), root)
            .expect("挂载成功");
        (rt, app, window, node)
    }

    /// 一帧文本链路：`refresh_text` → `compute_layout` → `layout_text`（与 `App::frame` 同序）。
    fn frame_text(app: &mut App, window: WindowId) -> crate::text::TextLayoutReport {
        app.refresh_text(window)
            .expect("窗口存在")
            .expect("shape 不报错");
        app.window_mut(window)
            .expect("窗口存在")
            .compute_layout()
            .expect("布局不报错");
        app.layout_text(window)
            .expect("窗口存在")
            .expect("layout_text 不报错")
    }

    /// 把字形图集钉在 32²（生产最小档是 512²）：几十个字形就能把「整页都是本 pass 工作集」
    /// 这一支真实触发出来，`grow` 无处可扩。
    fn constrain_glyph_atlas(app: &mut App) {
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (32, 32));
        atlas.cap_page_for_test(32);
        app.atlas = atlas;
        // 两级缓存必须同事务换代（`text::atlas` 的模块契约）。
        app.glyph_atlas.clear();
    }

    /// P1-①：容量不足时的第二阶段软失败必须**统计上抛**（`skipped_glyphs`）、**可观测**
    /// （`AtlasFull` 诊断）、**下一帧重试**（重新入队 + 待重排标记），并在连续多帧后**升级为
    /// 可见错误**（`AtlasExhausted`），升级后不得每帧都报错。
    #[test]
    fn a_constrained_atlas_reports_skips_and_retries_next_frame() {
        let (_rt, mut app, window, node) =
            text_app("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789", Some(400.0));
        constrain_glyph_atlas(&mut app);

        let first = frame_text(&mut app, window);
        assert!(
            first.rasterized > 0,
            "本帧必须真的光栅（report: {first:?}）"
        );
        assert!(
            first.skipped_glyphs > 0,
            "32² 的页放不下 36 个不同字形（report: {first:?}）"
        );
        assert!(
            first.skipped_glyphs <= first.rasterized,
            "skipped 恒 <= rasterized（report: {first:?}）"
        );
        assert!(
            app.text_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                TextDiagnostic::AtlasFull { skipped } if *skipped > 0
            )),
            "软失败必须可观测（diagnostics: {:?}）",
            app.text_diagnostics()
        );
        // 下一帧重试的闭环：节点已重新入队，且槽里记的代际是「待重排」标记（≠ 真实代际）。
        {
            let tree = app.window(window).expect("窗口存在").tree();
            assert_eq!(
                tree.text_layout_dirty_len(),
                1,
                "软失败的节点必须重新入下一帧的重排队列"
            );
            assert_ne!(
                tree.text_slot(node).expect("文本槽").laid_generation(),
                app.atlas.generation(),
                "软失败的行必须带「待重排」标记（否则下一帧会直接复用、永不补字形）"
            );
        }

        // 下一帧：同一节点必须**重排**（不是 reused），并且缺口仍然可观测。
        let retry = app
            .layout_text(window)
            .expect("窗口存在")
            .expect("第二次 pass 还没到升级阈值");
        assert_eq!(
            (retry.laid, retry.reused),
            (1, 0),
            "软失败的行下一帧必须真正重光栅重试（report: {retry:?}）"
        );
        assert!(
            retry.skipped_glyphs > 0,
            "容量没变，重试仍放不下（report: {retry:?}）"
        );

        // 连续 `ATLAS_FULL_ESCALATION_FRAMES` 个 pass 都软失败 ⇒ 可见错误。
        assert_eq!(ATLAS_FULL_ESCALATION_FRAMES, 3, "阈值是「连续 3 个 pass」");
        let third = app.layout_text(window).expect("窗口存在");
        let error = third.expect_err("连续软失败达到阈值必须升级为错误");
        assert!(
            matches!(
                error,
                EngineError::AtlasExhausted {
                    kind: AtlasKind::Glyph
                }
            ),
            "必须报 AtlasExhausted/Glyph，实际 {error:?}"
        );
        // 升级时计数清零：紧随其后的一个 pass 不能立刻再报错（否则等于每帧丢帧）。
        let fourth = app
            .layout_text(window)
            .expect("窗口存在")
            .expect("升级后计数清零，下一个 pass 不得再报错");
        assert!(
            fourth.skipped_glyphs > 0,
            "容量仍未恢复（report: {fourth:?}）"
        );
    }

    /// 宽裕场景：`skipped_glyphs == 0`，且不得压 `AtlasFull` 诊断、不得把节点重新入队。
    #[test]
    fn an_ample_atlas_reports_zero_skipped_glyphs() {
        let (_rt, mut app, window, _node) = text_app("hello", Some(200.0));

        let report = frame_text(&mut app, window);
        assert!(
            report.rasterized > 0,
            "本帧必须真的光栅（report: {report:?}）"
        );
        assert_eq!(
            report.skipped_glyphs, 0,
            "512² 的页放得下 5 个字形（report: {report:?}）"
        );
        assert!(
            !app.text_diagnostics()
                .iter()
                .any(|diagnostic| matches!(diagnostic, TextDiagnostic::AtlasFull { .. })),
            "没有软失败就不该有 AtlasFull 诊断（diagnostics: {:?}）",
            app.text_diagnostics()
        );
        let tree = app.window(window).expect("窗口存在").tree();
        assert_eq!(tree.text_layout_dirty_len(), 0, "没有软失败就不该重新入队");
    }

    /// 重试必须真的能补齐：容量恢复后同一节点重排，`skipped` 归零且 `ShapedLine` 的字形数补齐
    /// （证明「待重排」标记 + 重新入队确实是可用恢复路径，而不是永远重试永远缺字）。
    #[test]
    fn a_retried_line_fills_the_missing_glyphs_once_capacity_returns() {
        let text = "ABCDEFGHIJ0123456789";
        let (_rt, mut app, window, node) = text_app(text, Some(400.0));
        constrain_glyph_atlas(&mut app);
        let first = frame_text(&mut app, window);
        assert!(
            first.skipped_glyphs > 0,
            "受限图集必须触发软失败（report: {first:?}）"
        );

        // 容量恢复（模拟 `AtlasAllocator::grow` 成功后的下一档页）；两级缓存同事务换代。
        app.atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (512, 512));
        app.glyph_atlas.clear();
        let retried = app
            .layout_text(window)
            .expect("窗口存在")
            .expect("容量恢复后不得报错");
        assert_eq!(
            retried.skipped_glyphs, 0,
            "容量恢复后不得再软失败（report: {retried:?}）"
        );
        assert!(
            retried.laid >= 1,
            "软失败的行必须被重排（report: {retried:?}）"
        );
        let line = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .expect("行快照")
            .clone();
        let drawn: usize = line.glyph_runs().map(|run| run.glyphs.len()).sum();
        assert_eq!(
            drawn,
            text.chars().count(),
            "重试后必须把少画的字形全部补上（重排轮数上限 {MAX_LAYOUT_TEXT_PASSES}）"
        );
        assert_eq!(line.atlas_generation, app.atlas.generation());
    }
}

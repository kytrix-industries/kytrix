//! 帧推进：帧门禁前的 pass 调度、`frame` 主体、中止与空闲记账。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use super::App;
#[cfg(feature = "hud")]
use crate::debug_hud;
#[cfg(feature = "test-support")]
use crate::test_support::record::FrameStats;
#[cfg(feature = "test-support")]
use crate::test_support::record::LaneOutcome;
use crate::{
    error::EngineError,
    window::{FrameOutcome, FrameTicket, Window, WindowId},
};

/// 取走「需要一帧」的请求位（取走即清零）。
///
/// 消费者：`App::take_wake_request`（**crate 内部**：R01-16 起不再公开，应用的入口是
/// `App::request_repaint` / `App::request_node_repaint`）。
pub(crate) fn take_wake_request(app: &mut App) -> bool {
    let requested = app.wake_requested;
    app.wake_requested = false;
    requested
}

/// 置「需要一帧」并**唤醒平台事件循环**（合并位由 `Dispatcher::wake` 保证幂等）。
///
/// 不能只置 `wake_requested`：平台泵的顺序是「事件 → 钩子 → 帧 → a11y → 回投任务」，
/// 回投任务（`AppHandle::send`、a11y `ActionRequest`）里写状态时循环已经过了帧门禁、
/// 即将阻塞在 `GetMessageW` / `poll`。只置位不唤醒会让这批状态一直等到下一条无关消息
/// （画面与 a11y 更新都迟到）——P15 用 UIA 客户端实测 `SelectionItemPattern.Select()`
/// 不生效时定位到这一点。
/// 消费者：`App::request_frame`。
pub(crate) fn request_frame(app: &mut App) {
    app.wake_requested = true;
    app.dispatcher.wake();
}

// ── P06：完整帧（styles → text → layout → layout_text → paint → commit）────────

/// 完整帧：`refresh_styles` → `refresh_text` → `compute_layout`（仅 LAYOUT 脏）→ `layout_text`
/// → paint → commit。
///
/// 返回 `None` = 窗口不存在（**不**消费 wake 请求）；`Ok(FrameOutcome::Idle)` = 空闲零帧（I3，
/// 不提交、`FrameIndex` 不变）；`Err` = 本帧已中止（damage/dirty 保留，下一帧重试）。
///
/// 产品路径：含 `bind_style`/`bind_text` 的窗口只能走这里；`Window::frame` 是它的无 App 通道版本。
/// 消费者：`App::frame`。
pub(crate) fn frame(app: &mut App, window: WindowId) -> Option<Result<FrameOutcome, EngineError>> {
    if !app.windows.contains_key(&window) {
        return None;
    }
    // B6-2：合成线程的**内容错误**（clip 链超限等）在这里冒泡。编码已不在 UI 线程，错误只能
    // 经合成线程的错误槽回到 UI；本帧不再推进（错误是粘性的，调用方处理之后下一帧重试）。
    if let Some(error) = crate::render::compositor_thread::compositor().take_error() {
        app.record_idle(window);
        return Some(Err(error));
    }
    // R02-5：把未消费的事件在交给 `on_frame` 钩子之后再兜底有界化一次（正常推入就已经有界，
    // 这里是 O(1) 的防御；钩子可能持有上一轮的事件引用做重放，所以裁剪放在帧入口）。
    app.events.trim();
    #[cfg(feature = "hud")]
    app.prepare_debug_hud(window);
    // P2-3.3：偏移只在显式滚动时被夹紧，而可用范围 `content − viewport` 会随**内容尺寸**
    // 与**视口尺寸**变化 —— 两者都不一定触发重排（`set_scroll_content` 只写滚动槽），
    // 所以这里**每帧**都重夹一次。必须在取唤醒位**之前**：夹紧本身可能请求一帧，
    // 否则「内容变短」这类不标脏位的变化会停在 Idle 上永远不修。
    app.normalize_scroll_offsets(window);
    // 锚点定位的浮层按**上一帧实测尺寸**在这一帧布局之前纠位（P2-6）。不能放在布局之后：
    // 布局之后写 inset 会被 `settle_frame` 的「本轮已布局」判定清掉 LAYOUT，下一帧变 Idle，
    // 位置永远不生效。放在布局前则是标准的 measure-then-place：首帧用零尺寸，次帧到位。
    app.resolve_overlay_placements(window);
    let wake = app.take_wake_request();
    // P10：动画在帧门禁**之前**推进（写 `Transient` + 给注册节点标 `PAINT`），因此「有动画」必然
    // 让 `needs_frame` 为真；动画结束后不再留帧。
    app.tick_animations(window);
    let needs_frame = app
        .windows
        .get(&window)
        .is_some_and(|candidate| candidate.needs_frame(wake));
    if !needs_frame {
        app.record_idle(window);
        return Some(Ok(FrameOutcome::Idle));
    }
    let Some(ticket) = app.windows.get_mut(&window).and_then(Window::begin_frame) else {
        return Some(Ok(FrameOutcome::Idle));
    };
    let Some(styles) = app.refresh_styles(window) else {
        app.abort_frame(window, ticket);
        return None;
    };
    // P17：必须在 refresh_styles 之后写 HUD 文本，否则 `clear_paint` 会吞掉脏位；
    // 又必须在 refresh_text 之前，让新文本在同一帧完成 shape。
    #[cfg(feature = "hud")]
    app.apply_debug_hud(window);
    // R13-6：`bind_a11y` 的重算与像素管线解耦。`refresh_styles` 已把只绑 a11y 的节点的补丁
    // 重算完（它们不标 `BIND`/`PAINT`）；若本帧因此没有任何像素工作，就整段跳过文本/布局/
    // paint/编码（见下面的 a11y-only 出口）。
    if app
        .windows
        .get(&window)
        .is_some_and(Window::a11y_only_frame)
    {
        // a11y-only 帧：仍然"出帧"（平台泵在本轮之后照常投递 TreeUpdate、节流只管投递），
        // 但**不跑** paint 管线：不重建 lane、不编码、不产生 damage、`FrameIndex` 不变。
        app.abort_frame(window, ticket);
        #[cfg(feature = "test-support")]
        if let Some(target) = app.windows.get_mut(&window) {
            // 覆盖 `abort_frame` 记下的普通空闲帧：本帧的指标面是 a11y-only。
            target.record_a11y_only_frame();
        }
        return Some(Ok(FrameOutcome::Idle));
    }
    let text = match app.refresh_text(window) {
        Some(Ok(report)) => report,
        Some(Err(error)) => {
            app.abort_frame(window, ticket);
            return Some(Err(error));
        }
        None => {
            app.abort_frame(window, ticket);
            return None;
        }
    };
    // P1-2.6：`refresh_text` 可能已让图集换代（DPI 变化 / 字形容量逃逸）。图集是进程级
    // 共享的，页内 tile 已全部重排，而各窗口文本 lane 里存的是旧 `AtlasTile` —— 必须让
    // **所有窗口**的文本节点重绘。放在 `layout_text` 之前：本帧就会用新 UV 重建 lane。
    app.repaint_text_if_atlas_changed();
    let layout = match app.windows.get_mut(&window).map(Window::layout_if_dirty) {
        Some(Ok(report)) => report,
        Some(Err(error)) => {
            app.abort_frame(window, ticket);
            return Some(Err(error));
        }
        None => {
            app.abort_frame(window, ticket);
            return None;
        }
    };
    let text_layout = match app.layout_text(window) {
        Some(Ok(report)) => report,
        Some(Err(error)) => {
            app.abort_frame(window, ticket);
            return Some(Err(error));
        }
        None => {
            app.abort_frame(window, ticket);
            return None;
        }
    };
    // 布局后（视口尺寸已知/已变）再修一次虚拟列表窗口：首次布局前视口 = 0，不能靠
    // `set_virtual_items` 那一次调用定终身（P10 §8.3 / 评审 #5）。
    app.rewindow_virtuals(window);
    // 布局之后再夹一次滚动偏移：帧首那次用的是**上一帧**视口，窗口变大时上限变小，
    // 旧偏移会越界——本帧 paint 就用它，于是内容上移过多/底部空白上屏（复审 P2-8）。
    // 这里不请求帧：damage 已累积、本帧 paint 会消费它，除非真的没有帧可出。
    let clamped = app.clamp_scroll_offsets(window);
    if clamped {
        app.request_frame();
    }
    // 图集页上传（P13：逐窗口分发，两张页分开、顺序固定以便帧记录复现）。
    app.upload_atlas_pages();
    // 借用拆分：`windows` 与 `geometry` 是不同字段，可同时借出（几何表只读）。
    // 先取结果、结束后再处理错误 —— 解构 `self` 期间不能再调 `self.abort_frame`。
    let painted = {
        let App {
            windows, geometry, ..
        } = app;
        windows
            .get_mut(&window)
            .map(|target| target.paint_frame(geometry))
    };
    let paint = match painted {
        Some(Ok(report)) => report,
        Some(Err(error)) => {
            app.abort_frame(window, ticket);
            return Some(Err(error));
        }
        None => {
            app.abort_frame(window, ticket);
            return None;
        }
    };
    #[cfg(feature = "test-support")]
    {
        let Some(target) = app.windows.get(&window) else {
            app.abort_frame(window, ticket);
            return None;
        };
        let stats = FrameStats {
            styles,
            text: Some(text),
            text_layout: Some(text_layout),
            layout,
            painted_lanes: paint.rebuilt,
            reused_lanes: paint
                .log
                .lanes
                .iter()
                .filter(|entry| entry.outcome == LaneOutcome::Reused)
                .count(),
            primitives: target.scene().primitive_count(),
            instances: paint.instances,
            encoded_lanes: paint.encoded_lanes,
            reused_segments: paint.reused_segments,
            // 编码器侧的增量字节数；渲染器路径会用载荷的实际字节数覆盖它
            // （`Window::submit_gpu`；headless 保持编码器侧值）。
            uploaded_bytes: paint.uploaded_bytes,
            // B6-2 的三个合成观测字段：`submit_gpu` 在发布时按合成路径回填
            // （inline ⇒ encode_on_compositor = false；threaded ⇒ 读合成线程计数）。
            compositor_thread_wait: None,
            encode_on_compositor: false,
            scroll_offset_frames: 0,
            // R13-6 的累计指标：本帧是像素帧，数字保持不变（a11y-only 帧才会 +1）。
            a11y_only_frames: target.a11y_only_frames(),
        };
        let Some(target) = app.windows.get_mut(&window) else {
            app.abort_frame(window, ticket);
            return None;
        };
        target.set_frame_stats(stats);
    }
    #[cfg(not(feature = "test-support"))]
    {
        let _ = (&styles, &text, &text_layout, &layout);
    }
    let Some(target) = app.windows.get_mut(&window) else {
        app.abort_frame(window, ticket);
        return None;
    };
    let outcome = target.commit_frame(ticket, paint);
    // HUD 是纯观察者：提交成功后只清掉已经落进 paint 的待写文本（没有帧就保持上一次文本）。
    #[cfg(feature = "hud")]
    if matches!(outcome, FrameOutcome::Committed(_))
        && let Some(state) = app.debug_hud.get_mut(&window)
    {
        debug_hud::finish_frame(state);
    }
    // 图集行/整页没赶上本帧（窗口的共享槽还在消费上一笔）：这一帧会以旧图集呈现，因此
    // 立刻再要一帧（全量 damage），槽空后把内容补发——否则静态窗口会把缺字的画面一直
    // 留着。槽长期不空（GPU 卡住 / 窗口不再出帧）时最多补 `MAX_ATLAS_FOLLOWUPS` 帧，不空转。
    let glyph_extent = app.atlas.page().0;
    let image_extent = app.resources.page_pixels().map(|(page, _)| page.0);
    let deferred = app.windows.get(&window).is_some_and(|target| {
        use crate::paint::atlas::AtlasKind;
        target.has_renderer()
            && (target.atlas_rects_pending()
                || !target.atlas_page_matches(AtlasKind::Glyph, glyph_extent)
                || image_extent
                    .is_some_and(|extent| !target.atlas_page_matches(AtlasKind::Image, extent)))
    });
    if deferred {
        let retry = app
            .windows
            .get_mut(&window)
            .is_some_and(Window::note_atlas_deferral);
        if retry {
            if let Some(target) = app.windows.get_mut(&window) {
                target.request_repaint();
            }
            app.dispatcher.wake();
        }
    } else if let Some(target) = app.windows.get_mut(&window) {
        target.reset_atlas_deferrals();
    }
    Some(Ok(outcome))
}

/// 中止本帧（不结算、不递增 `FrameIndex`）。
/// 消费者：`App::abort_frame`。
pub(crate) fn abort_frame(app: &mut App, window: WindowId, ticket: FrameTicket) {
    if let Some(target) = app.windows.get_mut(&window) {
        target.abort_frame(ticket);
    }
}

/// 记录一次空闲帧（仅 `test-support`；其余构建是空操作）。
/// 消费者：`App::record_idle`。
pub(crate) fn record_idle(app: &mut App, window: WindowId) {
    #[cfg(feature = "test-support")]
    if let Some(target) = app.windows.get_mut(&window) {
        target.record_idle();
    }
    #[cfg(not(feature = "test-support"))]
    let _ = (app, window);
}

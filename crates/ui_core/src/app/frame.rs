//! 帧推进：帧门禁前的 pass 调度、`frame` 主体、中止与空闲记账。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use std::time::Instant;

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
/// `forced`：本帧走 **force 通道**（resize / 拖动 / 系统模态循环 / 动画 tick 等必须立即出帧
/// 的路径），绕过 [`App::frame`] 入口的「每 refresh 周期最多一帧」预算门禁（A5）。普通唤醒
/// （输入 / 行情写入 / 任务回投）传 `false`：快于刷新的请求合并到下一拍，最多晚 1 个 vsync。
///
/// 返回 `None` = 窗口不存在（**不**消费 wake 请求）；`Ok(FrameOutcome::Idle)` = 空闲零帧（I3，
/// 不提交、`FrameIndex` 不变）或本帧被预算门禁推迟（状态保留，平台一次性唤醒会补一帧）；
/// `Err` = 本帧已中止（damage/dirty 保留，下一帧重试）。
///
/// 产品路径：含 `bind_style`/`bind_text` 的窗口只能走这里；`Window::frame` 是它的无 App 通道版本。
/// 消费者：`App::frame`（预算门禁）/ `App::frame_forced`（force 通道）。
pub(crate) fn frame(
    app: &mut App,
    window: WindowId,
    forced: bool,
) -> Option<Result<FrameOutcome, EngineError>> {
    if !app.windows.contains_key(&window) {
        return None;
    }
    // B6-2：合成线程的**活性**与**内容错误**都在这里冒泡。
    //
    // 活性（supervision）：帧管线是单点 —— 合成线程一旦意外退出，UI 照常发布、GPU 线程空等、
    // 屏幕停在最后一帧，用户看到的就是「画面冻结而输入仍工作」。这里每帧入口监督一次：
    // 线程已死就重启，并让**所有**窗口整帧重发（新线程没有 lane 历史，增量载荷会画成退化
    // 实例）；重启失败 / 邮箱关闭则显式失败，绝不静默退化成空转。
    match crate::render::compositor_thread::compositor().supervise() {
        Ok(false) => {}
        Ok(true) => {
            let windows: Vec<WindowId> = app.windows.keys().copied().collect();
            for target in windows {
                if let Some(target) = app.windows.get_mut(&target) {
                    target.force_full_republish();
                }
            }
        }
        Err(error) => {
            app.record_idle(window);
            return Some(Err(error));
        }
    }
    // 合成线程的内容错误（clip 链超限等）：编码已不在 UI 线程，错误只能经合成线程的错误槽
    // 回到 UI；本帧不再推进（错误是粘性的，调用方处理之后下一帧重试）。
    if let Some(error) = crate::render::compositor_thread::compositor().take_error() {
        app.record_idle(window);
        return Some(Err(error));
    }
    // R02-5：把未消费的事件在交给 `on_frame` 钩子之后再兜底有界化一次（正常推入就已经有界，
    // 这里是 O(1) 的防御；钩子可能持有上一轮的事件引用做重放，所以裁剪放在帧入口）。
    // D-6：帧入口兜底轮询一次异步解码结果（`run_frame_hook` 的正常路径已在泵里轮询；
    // headless / 自驱帧宿主没有泵钩子，若这一帧前恰好有解码完成，这里交付并置出帧请求）。
    app.poll_resources();
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
    // A5：每 refresh 周期最多一帧的预算门禁（force 通道绕过）。位置在 `needs_frame` 之后：
    // 只有**真的有事情要做**的帧才配一次预算名额 —— 空闲帧（I3）在这里之前已经返回，
    // 不会被记成「推迟」而让平台误挂一次性唤醒（空闲时零唤醒不变式）。
    //
    // 被推迟时脏位 / damage 保持原样，**不**结算、不递增 `FrameIndex`：平台的一次性
    // 唤醒（Windows `FramePacer` / macOS 连续帧定时器 / Linux calloop timeout）到期后会再走一次
    // `App::frame`，那一帧补上本次状态（最多晚 1 个 vsync，用户裁决允许）。
    let now = Instant::now();
    let admits = app
        .windows
        .get(&window)
        .is_none_or(|candidate| candidate.frame_budget_admits(now, forced));
    if !admits {
        if let Some(target) = app.windows.get_mut(&window) {
            target.note_frame_budget_deferred();
        }
        record_budget_deferred(app, window);
        return Some(Ok(FrameOutcome::Idle));
    }
    if let Some(target) = app.windows.get_mut(&window) {
        target.note_frame_start(now);
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
    // D-5：图集代际在本帧 paint 之前取一次（`paint_frame` 的绘制前硬校验用）。
    let atlas_generation = app.atlas.generation();
    // 借用拆分：`windows` 与 `geometry` 是不同字段，可同时借出（几何表只读）。
    // 先取结果、结束后再处理错误 —— 解构 `self` 期间不能再调 `self.abort_frame`。
    let painted = {
        let App {
            windows, geometry, ..
        } = app;
        windows
            .get_mut(&window)
            .map(|target| target.paint_frame(geometry, Some(atlas_generation)))
    };
    let mut paint = match painted {
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
            compositor_inbox_latency: None,
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
    // D-5：绘制前硬校验跳过的旧代际文本行先取出来（commit_frame 会消费 paint）。
    let stale_text = core::mem::take(&mut paint.stale_text);
    let outcome = target.commit_frame(ticket, paint);
    // HUD 是纯观察者：提交成功后只清掉已经落进 paint 的待写文本（没有帧就保持上一次文本）。
    #[cfg(feature = "hud")]
    if matches!(outcome, FrameOutcome::Committed(_))
        && let Some(state) = app.debug_hud.get_mut(&window)
    {
        debug_hud::finish_frame(state);
    }
    // D-5：图集换代硬校验的补齐入口。被跳过的行本帧**没有编码旧 UV**；提交后逐节点重新入队，
    // 下一帧的 layout_text 用当前代际重光栅（laid_generation 失配 ⇒ 不命中复用）。
    if !stale_text.is_empty() {
        if let Some(target) = app.windows.get_mut(&window) {
            let tree = target.tree_mut();
            for node in stale_text {
                // 清 line + 置 laid_* 失配 + mark_paint：只 mark 脏位不够——若槽里的
                // laid_generation 恰好等于当前代际，layout_text 的复用判据仍会命中旧行。
                tree.invalidate_text_line(node);
            }
        }
        app.request_frame();
    }
    // 图集行/整页没赶上本帧（窗口的共享槽还在消费上一笔）：这一帧会以旧图集呈现，因此
    // 立刻再要一帧（全量 damage），槽空后把内容补发——否则静态窗口会把缺字的画面一直
    // 留着。槽长期不空（GPU 卡住 / 窗口不再出帧）时最多补 `MAX_ATLAS_FOLLOWUPS` 帧，不空转。
    let glyph_extent = app.atlas.page().0;
    // 图片图集是多页 texture array（D-9-c）：除 extent 之外，每层是否已整层初始化也必须对齐，
    // 否则新页 / 新窗口会采样到未定义层（表现为整块花图而不是「旧图集」）。
    let image_pages = app.resources.page_count();
    let image_extent = (image_pages > 0).then(|| app.resources.page_extent());
    let image_layers = image_pages as u16;
    let deferred = app.windows.get(&window).is_some_and(|target| {
        use crate::paint::atlas::AtlasKind;
        target.has_renderer()
            && (target.atlas_rects_pending()
                || !target.atlas_page_matches(AtlasKind::Glyph, glyph_extent)
                || image_extent.is_some_and(|extent| {
                    !target.atlas_page_matches(AtlasKind::Image, extent)
                        || (0..image_layers).any(|layer| !target.image_layer_received(layer))
                }))
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

/// 记录一次「被预算门禁推迟」的帧请求（仅 `test-support`；其余构建是空操作）。
///
/// 与空闲帧分开记录：`FrameRecord::skipped_by_budget` 让测试能区分「门禁推迟」与「掉帧」，
/// 这是 A5 的可断言面（P1A-5d）。
pub(crate) fn record_budget_deferred(app: &mut App, window: WindowId) {
    #[cfg(feature = "test-support")]
    if let Some(target) = app.windows.get_mut(&window) {
        target.record_budget_deferred();
    }
    #[cfg(not(feature = "test-support"))]
    let _ = (app, window);
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use std::time::Duration;

    use crate::{
        app::App,
        math::{Rect, Scale, Size},
        node::builder::NodeBuilder,
        paint::style::{PaintSpec, StylePatch},
        window::{FrameOutcome, WindowOptions},
    };

    /// 建一个 headless App + 一个可重复失效的绑定节点（每次 `write` 都产生一帧真正的像素工作）。
    fn app_with_bound_node() -> (
        App,
        crate::window::WindowId,
        crate::node::signal::Signal<u8>,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("frame budget"),
            size: Size::new(400.0, 300.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("窗口存在").tree().root();
        let value = app.signal(0u8);
        let source = value;
        let _node = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .paint(PaintSpec {
                background: crate::paint::color::Color::WHITE,
                ..PaintSpec::default()
            })
            .bind_style(move |cx| StylePatch {
                opacity: Some(f32::from(*cx.read(&source)) / 255.0),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
            .expect("绑定节点");
        (app, window, value)
    }

    fn committed(outcome: Option<Result<FrameOutcome, crate::error::EngineError>>) -> bool {
        matches!(outcome, Some(Ok(FrameOutcome::Committed(_))))
    }

    /// **P1A-5a 的核心断言**：同一 refresh 周期内的第二次请求被预算门禁合并成 `Idle`（脏位保留、
    /// `FrameIndex` 不变、没有掉帧），到期后同一份状态补出一帧；force 通道绕过门禁。
    ///
    /// 变异守卫：删掉 `app/frame.rs` 里的预算门禁（`frame_budget_admits` 分支）后，第二次调用会
    /// 直接 `Committed`，本测试必须变红。
    #[test]
    fn a_second_request_inside_one_refresh_period_is_deferred_then_resumed() {
        let (mut app, window, value) = app_with_bound_node();
        // 注入确定的 200 ms 周期：真实刷新率受机器负载影响，首帧偶发超过一个周期会让本测试
        // 变成测调度抖动（该机制本身用 24–480 Hz 的提示驱动，见 `window::frame_budget` 的
        // 单测）。
        app.window_mut(window)
            .expect("窗口存在")
            .set_frame_budget_test_interval(Duration::from_millis(200));

        // 首帧：有真实像素工作 ⇒ 提交。
        assert!(app.write(&value, 255));
        assert!(committed(app.frame(window)), "首帧必须提交");
        let first_index = app.window(window).expect("窗口存在").frame_index();
        assert_eq!(app.frame_budget_deferrals(window), 0);

        // 同一 refresh 周期内的第二次请求：推迟成 Idle。
        assert!(app.write(&value, 128));
        let deferred = app.frame(window);
        assert!(
            matches!(deferred, Some(Ok(FrameOutcome::Idle))),
            "同一 refresh 周期内的第二次请求必须被推迟"
        );
        assert_eq!(
            app.window(window).expect("窗口存在").frame_index(),
            first_index,
            "推迟不递增 FrameIndex（不是提交帧）"
        );
        assert_eq!(app.frame_budget_deferrals(window), 1, "推迟计数必须可观测");
        let record = app
            .window(window)
            .expect("窗口存在")
            .record()
            .expect("推迟也必须有帧记录")
            .clone();
        assert_eq!(record.skipped_by_budget, 1);
        assert!(!record.rendered, "推迟记录不是提交帧");

        // force 通道（resize / 拖动 / 动画 tick）：绕过预算，立即提交同一份状态。
        // Linux 的 present 门禁本身就是 `wl_surface.frame`，没有 force 调用点（`frame_forced`
        // 在该端不编译），因此这部分断言只在 Windows/macOS 上跑。
        #[cfg(any(windows, target_os = "macos"))]
        {
            let forced = app.frame_forced(window);
            assert!(committed(forced), "force 通道必须绕过帧预算门禁");
            assert!(
                app.window(window).expect("窗口存在").frame_index() > first_index,
                "force 帧必须真正提交"
            );
        }
        // force 帧推进了相位：紧接着的普通请求又被合并。
        assert!(app.write(&value, 64));
        assert!(matches!(app.frame(window), Some(Ok(FrameOutcome::Idle))));

        // 等到下一拍：同一份脏状态无需新写入即可补出一帧（P1A-5a 的「状态写入合并到下一 tick」）。
        std::thread::sleep(Duration::from_millis(220));
        assert!(
            committed(app.frame(window)),
            "下一拍必须把被推迟的状态补上（不是丢帧）"
        );

        // 空闲帧不参与预算：没有工作时不记推迟（I3「空闲零帧」不变式）。
        let before = app.frame_budget_deferrals(window);
        assert!(matches!(app.frame(window), Some(Ok(FrameOutcome::Idle))));
        assert_eq!(
            app.frame_budget_deferrals(window),
            before,
            "空闲帧不是预算推迟，不得虚增计数"
        );
    }

    /// **P1A-5b**：没有刷新率提示（headless / compositor 未提供）时门禁禁用，行为与 HEAD 一致。
    #[test]
    fn an_unknown_refresh_rate_leaves_the_frame_path_ungated() {
        let (mut app, window, value) = app_with_bound_node();
        assert_eq!(app.frame_refresh_hz(window), None);
        assert!(app.write(&value, 255));
        assert!(committed(app.frame(window)));
        assert!(app.write(&value, 128));
        assert!(
            committed(app.frame(window)),
            "刷新率未知时不得卡帧（退回唤醒即出帧）"
        );
        assert_eq!(app.frame_budget_deferrals(window), 0);
    }
}

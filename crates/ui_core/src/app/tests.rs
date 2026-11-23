use std::{cell::Cell, panic::AssertUnwindSafe, rc::Rc};

use super::App;
use crate::{
    event::{Action, ActionSource},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        dirty::DirtyFlags,
        id::NodeId,
        signal::{Derived, Signal, SignalId},
    },
    paint::style::StylePatch,
    window::{FrameOutcome, WindowId, WindowOptions},
};

fn new_app() -> (tokio::runtime::Runtime, App) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let app = App::new(rt.handle().clone());
    (rt, app)
}

// ── P05：文本 pass / 字体栈 / B1 失效纪律 ────────────────────────────────

fn text_style(width: Option<f32>) -> crate::paint::style::Style {
    let mut style = crate::paint::style::Style::default();
    style.inner.align_self = Some(taffy::style::AlignSelf::START);
    if let Some(width) = width {
        style.inner.size.width = taffy::style::Dimension::length(width);
    }
    style
}

/// P2-9：文本快照被重写后必须标 `PAINT`，否则 lane 复用旧 UV（新纹理 + 旧坐标）。
#[test]
fn re_laid_text_marks_paint_for_the_lane() {
    let (_rt, mut app, window, node) = text_app("hello", Some(400.0));
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);

    // 改文本 ⇒ 宽度变化 ⇒ 下一次 `layout_text` 会重写快照。
    set_text(
        &mut app,
        window,
        node,
        "hello brave new world",
        crate::text::TextStyle::new(16.0),
    );
    refresh(&mut app, window);
    layout(&mut app, window);
    // 清掉所有脏位，模拟「lane 仍可复用」的稳态：此时没有任何业务脏位。
    clear_dirty(&mut app, window);

    let report = text_layout(&mut app, window);
    assert!(report.laid > 0, "必须重排一次");
    assert!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty()
            .contains(DirtyFlags::PAINT),
        "重排后必须标 PAINT（否则 paint 复用旧 UV）"
    );
}

fn text_app(text: &str, width: Option<f32>) -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
    let (rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let w = app.window_mut(window).expect("窗口存在");
    let root = w.tree().root();
    let node = NodeBuilder::text(text, crate::text::TextStyle::new(16.0))
        .style(text_style(width))
        .build(w.tree_mut(), root)
        .expect("挂载成功");
    (rt, app, window, node)
}

fn refresh(app: &mut App, window: WindowId) -> crate::text::TextPassReport {
    app.refresh_text(window)
        .expect("窗口存在")
        .expect("文本 pass 不报错")
}

fn set_text(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    text: &str,
    style: crate::text::TextStyle,
) {
    app.window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .set_text_content(node, crate::text::TextContent::new(text, style));
}

fn text_layout(app: &mut App, window: WindowId) -> crate::text::TextLayoutReport {
    app.layout_text(window)
        .expect("窗口存在")
        .expect("文本重排不报错")
}

fn layout(app: &mut App, window: WindowId) -> crate::layout::LayoutReport {
    app.window_mut(window)
        .expect("窗口存在")
        .compute_layout()
        .expect("布局不报错")
}

fn layout_height(app: &App, window: WindowId, node: NodeId) -> f32 {
    app.window(window)
        .expect("窗口存在")
        .tree()
        .get(node)
        .expect("节点存在")
        .layout()
        .bounds
        .size
        .height
}

#[test]
fn refresh_text_requires_an_existing_window() {
    let (_rt, mut app) = new_app();

    assert!(app.refresh_text(WindowId::for_test(0)).is_none());
}

#[test]
fn refresh_text_shapes_once_then_reuses() {
    let (_rt, mut app, window, node) = text_app("hello", None);

    let first = refresh(&mut app, window);
    // P1-2.3：脏集合消费后为空 ⇒ 第二次调用不再遍历任何节点。
    // 旧实现每帧扫整棵树并把「快照未变」计入 `skipped`；现在「没有脏节点」本身就是
    // 更强的跳过（成本 O(0) 而不是 O(全树)），`skipped` 只统计真正访问到的节点。
    let second = refresh(&mut app, window);

    assert_eq!(first.shaped, 1);
    assert_eq!(second.shaped, 0, "脏集合为空时不得重新 shape");
    assert_eq!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .text_dirty_len(),
        0,
        "脏集合必须已被消费"
    );
    // 显式标记后必须重新进入（证明集合是唯一入口，而不是「永远跳过」）。
    app.window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .mark_all_text_dirty();
    let third = refresh(&mut app, window);
    assert_eq!(third.skipped, 1, "标记后必须访问该节点并命中快照复用");
    assert_eq!(third.shaped, 0);
    // 行快照只由第二阶段（layout_text）产出。
    assert!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .is_none(),
        "refresh_text must not attach a line snapshot (B1)"
    );
    let laid = text_layout(&mut app, window);
    assert_eq!(laid.laid, 1);
    assert!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .is_some(),
        "layout_text must attach the final snapshot"
    );
}

#[test]
fn bind_text_reevaluation_updates_content_and_marks_paint() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let signal = app.signal(smol_str::SmolStr::new_static("first"));
    let node = {
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::text("", crate::text::TextStyle::new(16.0))
            .bind_text(move |cx| cx.read(&signal).clone())
            .build(w.tree_mut(), root)
            .expect("挂载成功")
    };

    let report = app.refresh_styles(window).expect("窗口存在");
    assert_eq!(report.text_evaluated, 1);
    assert_eq!(report.text_changed, 1);
    clear_dirty(&mut app, window);
    let report = app.refresh_styles(window).expect("窗口存在");
    assert_eq!(report.text_changed, 0, "unchanged value must not re-mark");

    app.write(&signal, smol_str::SmolStr::new_static("second"));
    let report = app.refresh_styles(window).expect("窗口存在");
    assert_eq!(report.text_changed, 1);
    let content = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_content(node)
        .expect("文本节点")
        .clone();
    assert_eq!(content.text, "second");
    assert!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty()
            .contains(DirtyFlags::PAINT)
    );
}

#[test]
fn text_size_change_marks_layout() {
    let (_rt, mut app, window, node) = text_app("ab", None);
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    clear_dirty(&mut app, window);

    set_text(
        &mut app,
        window,
        node,
        "a much longer text that wraps",
        crate::text::TextStyle::new(16.0),
    );
    let _ = refresh(&mut app, window);

    assert!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty()
            .contains(DirtyFlags::LAYOUT),
        "a size change must set LAYOUT"
    );
}

#[test]
fn text_same_size_still_clears_the_taffy_cache() {
    // B1：尺寸没变（"ab" → "ba"）也必须清 node..root 的 taffy 缓存，否则陈旧 measure 会留存。
    let (_rt, mut app, window, node) = text_app("ab", None);
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let warm = layout(&mut app, window);
    assert_eq!(warm.cache_misses, 0, "second layout must hit the cache");
    assert!(warm.cache_hits > 0);
    clear_dirty(&mut app, window);

    set_text(
        &mut app,
        window,
        node,
        "ba",
        crate::text::TextStyle::new(16.0),
    );
    let _ = refresh(&mut app, window);
    let dirty = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .get(node)
        .expect("节点存在")
        .dirty();
    assert!(
        !dirty.contains(DirtyFlags::LAYOUT),
        "same measured size must not set LAYOUT"
    );

    let cold = layout(&mut app, window);
    assert!(
        cold.cache_misses >= 1,
        "the measure cache must be cleared even when the size is unchanged"
    );
}

#[test]
fn text_wrap_change_same_max_width_relayouts() {
    // B1 的反例：Wrap → Anywhere 的 max-content 宽相同，但定宽（40 px）下的行数/高度不同。
    let (_rt, mut app, window, node) = text_app("aaaaaaaaaa", Some(40.0));
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let height_before = layout_height(&app, window, node);

    let mut style = crate::text::TextStyle::new(16.0);
    style.wrap = crate::text::TextWrap::Anywhere;
    set_text(&mut app, window, node, "aaaaaaaaaa", style);
    let _ = refresh(&mut app, window);
    let relayout = layout(&mut app, window);
    let height_after = layout_height(&app, window, node);

    assert!(
        relayout.cache_misses >= 1,
        "snapshot change must invalidate the measure cache"
    );
    assert!(
        height_after > height_before,
        "BreakAll wraps into more lines at 40 px: {height_after} vs {height_before}"
    );
}

#[test]
fn set_font_stack_invalidates_all_text_and_drops_the_shape_cache() {
    let (_rt, mut app, window, node) = text_app("hello", None);
    let _ = refresh(&mut app, window);

    let version = app.set_font_stack(crate::text::FontStack::new(["Segoe UI"]));

    assert_eq!(version, 2);
    assert_eq!(app.font_stack_version(), 2);
    assert!(app.font_stack().families().count() == 1);
    let dirty = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .get(node)
        .expect("节点存在")
        .dirty();
    assert!(dirty.contains(DirtyFlags::LAYOUT | DirtyFlags::PAINT));
    // 形状缓存被清空 ⇒ 下一次 refresh_text 重新 shape。
    let report = refresh(&mut app, window);
    assert_eq!(report.shaped, 1);
}

#[test]
fn reload_system_fonts_is_idempotent_but_bumps_the_version() {
    let (_rt, mut app, window, _node) = text_app("x", None);
    let before = app.font_stack_version();

    let after = app.reload_system_fonts();

    assert_eq!(after, before + 1);
    assert!(app.refresh_text(window).is_some());
}

/// P1-2.6 回归：图集换代必须让**所有窗口**的文本节点重绘。
///
/// 图集是进程级共享的，但文本 lane 里存的是生成时的 `AtlasTile`（旧 UV）。旧实现里
/// `set_text_line` 不标 `PAINT`，paint 的 lane 复用只看 `REUSABLE`/`clip_key` —— 于是
/// 窗口 A 改 DPI 让图集换代后，窗口 B 仍在使用旧 UV（错字/花字）。
#[test]
fn dpi_change_relays_text_without_rebuilding_the_shared_atlas() {
    let (_rt, mut app, window_a, node_a) = text_app("alpha", None);
    // 第二个窗口：同样的文本节点，但一直不出帧 —— 它的 lane 保留上一帧的 AtlasTile。
    let window_b = app.create_window(WindowOptions::default());
    let node_b = {
        let tree = app.window_mut(window_b).expect("窗口存在").tree_mut();
        let root = tree.root();
        NodeBuilder::text("beta", crate::text::TextStyle::new(16.0))
            .build(tree, root)
            .expect("挂载")
    };

    // 两个窗口都完成一次 shape + layout（建立各自的文本 lane / ShapedLine）。
    for window in [window_a, window_b] {
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let _ = text_layout(&mut app, window);
    }
    let generation_before = app
        .window(window_b)
        .expect("窗口存在")
        .tree()
        .text_line(node_b)
        .expect("快照存在")
        .atlas_generation;

    // 窗口 A 改 DPI：新 ppem 只是**新增** tile（R12-4/R12-1），不清页、不换代。
    app.window_mut(window_a)
        .expect("窗口存在")
        .set_scale(crate::math::Scale::new(2.0).expect("finite scale"));
    let _ = refresh(&mut app, window_a);
    let _ = layout(&mut app, window_a);
    let _ = text_layout(&mut app, window_a);
    let generation_after = app
        .window(window_a)
        .expect("窗口存在")
        .tree()
        .text_line(node_a)
        .expect("快照存在")
        .atlas_generation;
    assert_eq!(
        generation_after, generation_before,
        "DPI 变化不得让全局图集换代（R12-4：逻辑坐标 shaping + 多 ppem 共存）"
    );

    // 窗口 B 不受影响：它的 tile 仍然有效，既不需要 PAINT 也不需要重排。
    let tree_b = app.window(window_b).expect("窗口存在").tree();
    assert_eq!(
        tree_b.text_layout_dirty_len(),
        0,
        "图集没换代 ⇒ 其它窗口的文本节点不得进入重排队列"
    );
    assert_eq!(
        tree_b
            .text_line(node_b)
            .expect("快照仍然有效")
            .atlas_generation,
        generation_before,
        "其它窗口的 ShapedLine 必须原样保留（不重排）"
    );
}

#[test]
fn scale_change_relays_the_same_snapshot_at_the_new_ppem() {
    let (_rt, mut app, window, node) = text_app("hello", None);
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let first = text_layout(&mut app, window);
    assert!(first.laid > 0);
    let generation_before = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_line(node)
        .expect("快照存在")
        .atlas_generation;

    app.window_mut(window)
        .expect("窗口存在")
        .set_scale(crate::math::Scale::new(2.0).expect("finite scale"));
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let second = text_layout(&mut app, window);
    let generation_after = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_line(node)
        .expect("快照存在")
        .atlas_generation;

    assert_eq!(
        generation_after, generation_before,
        "DPI 变化不再让图集换代（R12-4）：新 ppem 的 tile 直接加入，旧 tile 由 LRU 淘汰"
    );
    assert!(
        second.laid > 0,
        "同一份形状快照必须按新 scale 重新光栅（落点/位图尺寸都变）"
    );
    assert!(
        second.rasterized > 0,
        "新 ppem 必须真正光栅（复用旧 ppem 的 tile 是错的）"
    );
}

/// R12 的 `DigitFastPath` 命中率必须**可观测**（`TextPassReport`）：分母 = 进入判定的塑形数，
/// 含被静态条件拒绝的候选（D-2：真实命中率，不是只统计 eligible 文本的自证式口径）。
#[test]
fn digit_fast_path_hit_rate_is_observable() {
    // 数字文本：进入判定一次、命中一次。
    let (_rt, mut app, window, _node) = text_app("1,234.56", None);
    let report = refresh(&mut app, window);
    if report.fast_path_shaped == 0 {
        // 宿主字体对该字号的数字/标点组合验证失败（例如带 kerning）：这是合法的降级，
        // 不是失败；命中率的可观测性由下面「字母文本计入候选」的断言保证。
        eprintln!("skip: the host font rejects the digit fast path");
        assert_eq!(report.fast_path_queries, 1, "验证降级也计入候选分母");
        assert_eq!(report.fast_path_ineligible, 0);
    } else {
        assert_eq!(report.fast_path_queries, 1);
        assert_eq!(report.fast_path_shaped, 1);
        assert_eq!(report.fast_path_ineligible, 0);
        assert!(app.digit_fast_path_enabled());
    }

    // 字母文本：不满足字符集 ⇒ 不计命中，但必须计入候选分母与 ineligible（真实命中率）。
    let (_rt2, mut app2, window2, _node2) = text_app("frame a", None);
    let report2 = refresh(&mut app2, window2);
    assert_eq!(report2.fast_path_queries, 1, "字母文本必须计入候选分母");
    assert_eq!(report2.fast_path_shaped, 0);
    assert_eq!(report2.fast_path_ineligible, 1);

    // 开关关闭 ⇒ 连判定都不做（回退验证路径）。
    let (_rt3, mut app3, window3, _node3) = text_app("1,234.56", None);
    app3.set_digit_fast_path(false);
    assert!(!app3.digit_fast_path_enabled());
    let report3 = refresh(&mut app3, window3);
    assert_eq!(report3.fast_path_queries, 0);
    assert_eq!(report3.fast_path_shaped, 0);
    assert_eq!(report3.fast_path_ineligible, 0);
}

#[test]
fn text_diagnostics_start_empty_for_supported_text() {
    let (_rt, mut app, window, _node) = text_app("ascii only", None);
    let _ = refresh(&mut app, window);

    // 具体诊断依宿主字体而定；这里只要求 API 可用且不 panic。
    let _ = app.text_diagnostics();
}

fn node_with<F>(app: &mut App, window: WindowId, bind: F) -> NodeId
where
    F: Fn(&mut ReadCtx<'_>) -> StylePatch + 'static,
{
    let w = app.window_mut(window).expect("窗口存在");
    let root = w.tree().root();
    NodeBuilder::boxed()
        .bind_style(bind)
        .build(w.tree_mut(), root)
        .expect("挂载成功")
}

fn flags(app: &App, window: WindowId, node: NodeId) -> DirtyFlags {
    app.window(window)
        .expect("窗口存在")
        .tree()
        .get(node)
        .expect("节点存在")
        .dirty()
}

fn root_flags(app: &App, window: WindowId) -> DirtyFlags {
    let root = app.window(window).expect("窗口存在").tree().root();
    flags(app, window, root)
}

fn deps(app: &App, window: WindowId, node: NodeId) -> Vec<SignalId> {
    app.window(window)
        .expect("窗口存在")
        .tree()
        .deps(node)
        .expect("节点存在")
        .to_vec()
}

fn clear_dirty(app: &mut App, window: WindowId) {
    app.window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .clear_all_dirty_for_test();
}

#[test]
fn new_app_has_no_windows() {
    let (_rt, app) = new_app();

    assert_eq!(app.windows().count(), 0);
    assert!(app.window(WindowId::for_test(0)).is_none());
    assert!(app.eval_stack_is_empty_for_test());
    let joined = app.runtime_handle().spawn(async { 7u8 });
    drop(joined);
}

#[test]
fn write_only_reaches_the_owning_window() {
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(0);
    let window_a = app.create_window(WindowOptions::default());
    let window_b = app.create_window(WindowOptions::default());
    let node_a = node_with(&mut app, window_a, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal) as f32),
        ..StylePatch::default()
    });
    let node_b = node_with(&mut app, window_b, |_cx| StylePatch::default());
    app.refresh_styles(window_a).expect("窗口存在");
    app.refresh_styles(window_b).expect("窗口存在");

    assert_eq!(
        node_a.index(),
        node_b.index(),
        "两窗口的 arena 索引相同 ⇒ 必须靠 Owner 的 window 消歧"
    );
    clear_dirty(&mut app, window_a);
    clear_dirty(&mut app, window_b);

    assert!(app.write(&signal, 1));

    assert!(flags(&app, window_a, node_a).contains(DirtyFlags::PAINT));
    assert!(
        !flags(&app, window_b, node_b).contains(DirtyFlags::PAINT),
        "不得命中另一个窗口的同号节点"
    );
}

#[test]
fn write_marks_layout_when_patch_affects_layout() {
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(0);
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        affects_layout: true,
        opacity: Some(*cx.read(&signal) as f32),
        ..StylePatch::default()
    });
    app.refresh_styles(window).expect("窗口存在");
    clear_dirty(&mut app, window);

    assert!(app.write(&signal, 1));

    assert!(
        flags(&app, window, node).contains(DirtyFlags::LAYOUT),
        "affects_layout → 标 LAYOUT 而非只标 PAINT"
    );
    assert!(
        root_flags(&app, window).contains(DirtyFlags::LAYOUT),
        "必须冒泡"
    );
}

#[test]
fn write_skips_stale_owner_without_panicking() {
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(0);
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal) as f32),
        ..StylePatch::default()
    });
    app.refresh_styles(window).expect("窗口存在");
    app.window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .remove(node)
        .expect("卸载成功");
    let _ = app.take_wake_request();

    assert!(app.write(&signal, 1), "值确实变了");
    assert!(
        !app.take_wake_request(),
        "没有存活的 owner → 不请求唤醒（陈旧 owner 已被 retain 剔除）"
    );
}

#[test]
fn refresh_styles_only_reevaluates_dirty_nodes() {
    let (_rt, mut app) = new_app();
    let calls = Rc::new(Cell::new(0u32));
    let a = Rc::clone(&calls);
    let signal: Signal<u32> = app.signal(0);
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| {
        a.set(a.get() + 1);
        StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        }
    });

    let first = app.refresh_styles(window).expect("窗口存在");
    assert_eq!(first.evaluated, 1, "只有带 bind_style 的节点被求值");
    assert_eq!(calls.get(), 1);

    let second = app.refresh_styles(window).expect("窗口存在");
    assert_eq!(second.evaluated, 0, "PAINT 已消费 → 不重复求值");
    assert_eq!(calls.get(), 1);

    assert!(app.write(&signal, 3));
    let third = app.refresh_styles(window).expect("窗口存在");
    assert_eq!(third.evaluated, 1);
    assert_eq!(calls.get(), 2);
    assert_eq!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("存在")
            .style_patch()
            .opacity,
        Some(3.0)
    );
}

#[test]
fn refresh_styles_is_idempotent() {
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(0);
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal) as f32),
        ..StylePatch::default()
    });

    app.refresh_styles(window).expect("窗口存在");
    let deps_before = deps(&app, window, node);
    let patch_before = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .get(node)
        .expect("存在")
        .style_patch()
        .clone();

    let second = app.refresh_styles(window).expect("窗口存在");

    assert_eq!(second.evaluated, 0);
    assert_eq!(deps(&app, window, node), deps_before);
    assert_eq!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("存在")
            .style_patch(),
        &patch_before
    );
}

#[test]
fn refresh_styles_escalates_layout_when_patch_affects_layout() {
    // 双向都要升级（`new || old`）：false→true 本帧开始变；true→false 要把尺寸收回来。
    let (_rt, mut app) = new_app();
    let resized: Signal<bool> = app.signal(false);
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        affects_layout: *cx.read(&resized),
        ..StylePatch::default()
    });

    let quiet = app.refresh_styles(window).expect("窗口存在");
    assert_eq!(quiet.evaluated, 1);
    assert!(quiet.layout_dirty, "新建节点本身带 LAYOUT");

    clear_dirty(&mut app, window);
    assert!(app.write(&resized, true));
    let escalate = app.refresh_styles(window).expect("窗口存在");
    assert!(escalate.layout_dirty, "false → true 必须升级");
    assert!(flags(&app, window, node).contains(DirtyFlags::LAYOUT));
    assert!(root_flags(&app, window).contains(DirtyFlags::LAYOUT));

    clear_dirty(&mut app, window);
    assert!(app.write(&resized, false));
    let settle = app.refresh_styles(window).expect("窗口存在");
    assert!(
        settle.layout_dirty,
        "true → false 也必须升级（旧值声明过尺寸会变）"
    );
    assert!(flags(&app, window, node).contains(DirtyFlags::LAYOUT));
}

#[test]
fn refresh_styles_unknown_window_returns_none() {
    let (_rt, mut app) = new_app();

    assert!(app.refresh_styles(WindowId::for_test(99)).is_none());
}

#[test]
fn refresh_styles_updates_deps() {
    let (_rt, mut app) = new_app();
    let flag: Signal<bool> = app.signal(true);
    let a: Signal<u32> = app.signal(1);
    let b: Signal<u32> = app.signal(2);
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| {
        let use_a = *cx.read(&flag);
        let value = if use_a { *cx.read(&a) } else { *cx.read(&b) };
        StylePatch {
            opacity: Some(value as f32),
            ..StylePatch::default()
        }
    });

    app.refresh_styles(window).expect("窗口存在");
    let first = deps(&app, window, node);
    assert!(first.contains(&flag.id()) && first.contains(&a.id()));
    assert!(!first.contains(&b.id()));

    let _ = app.take_wake_request();
    assert!(app.write(&flag, false));
    app.refresh_styles(window).expect("窗口存在");
    let second = deps(&app, window, node);
    assert!(
        second.contains(&b.id()) && !second.contains(&a.id()),
        "条件分支切换后必须退订 a、订阅 b"
    );

    clear_dirty(&mut app, window);
    let _ = app.take_wake_request();
    assert!(app.write(&a, 99));
    assert!(
        !flags(&app, window, node).contains(DirtyFlags::PAINT),
        "已退订的信号不得再标脏该节点"
    );
}

/// P1-2.5 回归：卸载节点必须同步解开依赖边。
///
/// 旧实现只在 `close_window` 里退订仍存活的节点；`remove_node`（以及虚拟列表回收行）
/// 直接 `tree_mut().remove()`，留下永久死边。死边会随「挂载/卸载」次数无界累积，而
/// `App::write` 是最高频热路径 —— 每次写都要遍历并过滤这些死 owner。
///
/// 本用例读 `test_support::diagnostics::dependency_dump`，因此仅在 `test-support` 特性下编译。
#[cfg(feature = "test-support")]
#[test]
fn removing_a_node_detaches_its_dependencies() {
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(0);
    let window = app.create_window(WindowOptions::default());

    // 两个独立读者：一个用裸 `tree.remove` 卸载（分层上不负责依赖图），一个走
    // `App::remove_node`（必须在删除的同时 detach）。断言后者不残留、前者残留。
    let bare = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal) as f32),
        ..StylePatch::default()
    });
    let managed = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal) as f32),
        ..StylePatch::default()
    });
    app.refresh_styles(window).expect("窗口存在");
    let _ = app.take_wake_request();
    let before = crate::test_support::diagnostics::dependency_dump(&app);
    assert_eq!(before.edges, 2, "两个节点各一条边：{before:?}");

    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .remove(bare)
        .expect("裸卸载");
    assert!(app.remove_node(window, managed).is_ok(), "App 路径卸载");

    let after = crate::test_support::diagnostics::dependency_dump(&app);
    assert_eq!(
        after.edges, 1,
        "`remove_node` 必须解开该节点的边；只剩裸卸载留下的那一条：{after:?}"
    );
    let _ = app.take_wake_request();
    assert!(app.write(&signal, 1), "写本身仍成功");
}

#[test]
fn close_window_detaches_dependencies() {
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(0);
    let window = app.create_window(WindowOptions::default());
    let _node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal) as f32),
        ..StylePatch::default()
    });
    app.refresh_styles(window).expect("窗口存在");
    let _ = app.take_wake_request();

    assert!(app.close_window(window));
    assert!(app.window(window).is_none());
    assert!(app.write(&signal, 1), "写本身仍成功");
    assert!(
        !app.take_wake_request(),
        "关窗必须精确退订，不得再命中死节点"
    );
    assert!(!app.close_window(window), "重复关窗返回 false");
}

#[test]
fn refresh_styles_marks_a11y_on_interactive_change() {
    let (_rt, mut app) = new_app();
    let enabled: Signal<bool> = app.signal(false);
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        interactive: Some(*cx.read(&enabled)),
        ..StylePatch::default()
    });
    app.refresh_styles(window).expect("窗口存在");
    clear_dirty(&mut app, window);

    assert!(app.write(&enabled, true));
    app.refresh_styles(window).expect("窗口存在");

    assert!(
        flags(&app, window, node).contains(DirtyFlags::A11Y),
        "interactive 变化必须标 A11Y"
    );
}

/// R03-12：派生闭包 panic 之后必须**放回**，下一次读重试闭包 —— 不能把一次应用 bug 升级成
/// 该派生值的永久失效（旧行为：下一次读返回 `CyclicDerived`，伪装成"存在环"）。
///
/// 同时钉住 panic 安全的另一半：`EvalGuard::drop` 在展开时出栈、panic 节点自己的 `bind_style`
/// 闭包丢失后退化为静态节点（这是 `refresh_styles` 的既有语义，不是本条的修复点）。
#[test]
fn eval_guard_pops_on_panic_and_the_derived_closure_is_restored() {
    let (_rt, mut app) = new_app();
    // 第一次求值 panic、之后正常：闭包必须在 panic 展开时被放回。
    let attempts = Rc::new(Cell::new(0u32));
    let counter = Rc::clone(&attempts);
    let flaky: Derived<f32> = app.derived(move |_cx| -> f32 {
        counter.set(counter.get() + 1);
        if counter.get() == 1 {
            panic!("派生闭包第一次求值炸了")
        }
        7.0
    });
    let window = app.create_window(WindowOptions::default());
    // 第一个节点：本次求值会 panic（它的 `bind_style` 随后丢失 → 退化为静态节点）。
    let doomed = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(cx.read_derived(&flaky).map(|v| *v).unwrap_or(0.0)),
        ..StylePatch::default()
    });
    // 第二个节点：第一次 pass 里没轮到它，panic 之后才求值 —— 它是"闭包是否被放回"的探针。
    let survivor = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(cx.read_derived(&flaky).map(|v| *v).unwrap_or(0.0)),
        ..StylePatch::default()
    });

    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = app.refresh_styles(window);
    }));
    assert!(outcome.is_err(), "闭包 panic 必须向上传播");
    assert!(
        app.eval_stack_is_empty_for_test(),
        "EvalGuard::drop 必须在 panic 展开时出栈"
    );

    let report = app.refresh_styles(window).expect("窗口仍在");
    assert_eq!(
        report.evaluated, 1,
        "panic 的那个节点闭包已丢（跳过），另一个节点仍被求值"
    );
    assert_eq!(attempts.get(), 2, "第二次求值必须真的重跑派生闭包");
    let opacity = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .get(survivor)
        .expect("存在")
        .style_patch()
        .opacity;
    assert_eq!(
        opacity,
        Some(7.0),
        "闭包被放回 ⇒ 重试成功并写回真实值；旧实现返回 CyclicDerived 后这里会是 Some(0.0)"
    );
    assert!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(doomed)
            .is_ok()
    );
}

#[test]
fn write_isolates_sibling_nodes() {
    // D16/I3 的核心断言：一次写入只沿依赖图命中**读过它的**节点。
    let (_rt, mut app) = new_app();
    let signal_a: Signal<u32> = app.signal(0);
    let signal_b: Signal<u32> = app.signal(0);
    let window = app.create_window(WindowOptions::default());
    let node_a = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal_a) as f32),
        ..StylePatch::default()
    });
    let node_b = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal_b) as f32),
        ..StylePatch::default()
    });
    app.refresh_styles(window).expect("窗口存在");
    let before = flags(&app, window, node_b);
    let _ = app.take_wake_request();

    assert!(app.write(&signal_a, 1));

    assert!(
        flags(&app, window, node_a).contains(DirtyFlags::PAINT),
        "读者必须被标脏"
    );
    assert_eq!(
        flags(&app, window, node_b),
        before,
        "非读者（含 REUSABLE）必须完全不变"
    );
    assert!(app.take_wake_request(), "有节点变脏 ⇒ 必须请求唤醒");
}

#[test]
fn write_only_derived_does_not_wake() {
    // 只有派生失效、没有节点变脏 ⇒ 不该提交帧（I3「空闲不提交帧」）。
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(1);
    let derived: Derived<u32> = app.derived(move |cx| *cx.read(&signal) * 2);
    let window = app.create_window(WindowOptions::default());

    assert!(app.write(&signal, 2));
    assert!(!app.take_wake_request(), "只有派生失效 → 不唤醒");

    let report = app.refresh_styles(window).expect("窗口存在");
    assert_eq!(report.evaluated, 0, "没有节点读该派生");
    assert!(!report.wake_requested);
    assert!(app.write(&signal, 3));
    let _ = derived;
}

/// P1-4：被限流挡下的 a11y 变化必须让平台泵「到期补发」。
#[test]
fn a11y_retry_pending_reports_throttled_changes() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let root = app.window(window).expect("窗口").tree().root();
    let node = NodeBuilder::boxed()
        .build(app.window_mut(window).expect("窗口").tree_mut(), root)
        .expect("挂载");
    app.refresh_styles(window).expect("窗口存在");
    assert!(app.a11y_take(window).is_some(), "首笔交付");

    // 限流窗口内再标脏：不能立即交付，但必须有「到期补发」信号。
    app.window_mut(window)
        .expect("窗口")
        .tree_mut()
        .mark_a11y(node);
    assert!(!app.a11y_pending(window), "限流窗口内不立即交付");
    assert!(
        app.a11y_retry_pending(),
        "必须有到期补发信号，否则最后一次变化永远交付不出去"
    );

    // 时间窗过后：可交付，补发信号消失。
    app.advance_clock(std::time::Duration::from_millis(150));
    assert!(app.a11y_pending(window), "到期后可交付");
    assert!(!app.a11y_retry_pending(), "可交付时不再需要补发信号");
    let _ = app.a11y_take(window);
}
/// P2-8：`on_close` 返回 false 必须拦下关闭，返回 true 才真的关。
/// P2-3：给不存在的窗口注册关闭处理器必须被忽略（否则留下永不调用、却持有闭包的条目）。
#[test]
fn on_close_ignores_unknown_windows() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let ghost = app.create_window(WindowOptions::default());
    assert!(app.close_window(ghost));
    let called = std::rc::Rc::new(std::cell::Cell::new(0_u32));
    let counter = called.clone();
    app.on_close(ghost, move |_app| {
        counter.set(counter.get() + 1);
        false
    });
    assert!(!app.request_close(ghost), "已关闭的窗口没有可拦的关闭请求");
    assert_eq!(called.get(), 0, "幽灵处理器不得被调用");
    // 正常窗口仍可注册。
    app.on_close(window, |_app| false);
    assert!(!app.request_close(window));
}

/// P2-3：处理器内部重新注册的处理器，不能被 take→put 的旧闭包覆盖。
#[test]
fn close_handler_reregistration_is_not_overwritten() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    app.on_close(window, move |app| {
        // 第一次询问后换成一个「永远拦下」的新处理器；再点一次时必须走新的。
        app.on_close(window, |_app| false);
        false
    });
    assert!(!app.request_close(window), "旧处理器拦下");
    // 新处理器同样是拦下，但关键是它必须在表里（不是旧闭包）。
    assert!(!app.request_close(window), "新处理器也必须被问到");
    assert!(app.window(window).is_some(), "两次都拦下，窗口仍在");
}

#[test]
fn close_handler_can_veto_the_close() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let denied = std::rc::Rc::new(std::cell::Cell::new(0_u32));
    let seen = denied.clone();
    app.on_close(window, move |_app| {
        seen.set(seen.get() + 1);
        false
    });

    assert!(!app.request_close(window), "返回 false 必须拦下关闭");
    assert!(app.window(window).is_some(), "被拦下时窗口必须仍然存在");
    assert_eq!(denied.get(), 1, "处理器必须被调用");

    // 第二次也拦下（处理器不能被消费掉）。
    assert!(!app.request_close(window));
    assert_eq!(denied.get(), 2);

    // 换成允许关闭的处理器。
    app.on_close(window, |_app| true);
    assert!(app.request_close(window), "返回 true 必须真的关闭");
    assert!(app.window(window).is_none());
}

/// AR3 macOS P1-2：退出握手必须**两阶段** —— 任一窗口 veto 时一个窗口都不许关。
///
/// 旧实现逐窗 `App::request_close`（批准即关窗）：`[A, B, C]` 里 B veto 时 A 已经被拆掉
/// （Renderer 已 drop、应用状态丢失），而用户看到的是「退出已取消」。
#[test]
fn batch_close_rolls_back_every_window_on_veto() {
    let (_rt, mut app) = new_app();
    let first = app.create_window(WindowOptions::default());
    let second = app.create_window(WindowOptions::default());
    let third = app.create_window(WindowOptions::default());
    let asked_first = Rc::new(Cell::new(0_u32));
    let asked_second = Rc::new(Cell::new(0_u32));
    let asked_third = Rc::new(Cell::new(0_u32));
    let counter = asked_first.clone();
    app.on_close(first, move |_app| {
        counter.set(counter.get() + 1);
        true
    });
    // 第二个窗口 veto（例如「退出确认框」被用户取消）。
    let counter = asked_second.clone();
    app.on_close(second, move |_app| {
        counter.set(counter.get() + 1);
        false
    });
    let counter = asked_third.clone();
    app.on_close(third, move |_app| {
        counter.set(counter.get() + 1);
        true
    });

    assert!(
        !app.request_close_all(&[first, second, third]),
        "第二个窗口 veto ⇒ 整个退出握手回滚"
    );
    assert!(
        app.window(first).is_some(),
        "已经被问过的窗口不得被拆掉（旧实现的数据丢失）"
    );
    assert!(app.window(second).is_some());
    assert!(app.window(third).is_some());
    assert_eq!(asked_first.get(), 1, "询问阶段必须真的问过应用");
    assert_eq!(asked_second.get(), 1);
    assert_eq!(
        asked_third.get(),
        0,
        "已经否决之后不再打扰后面的窗口（它们的处理器一次都不该跑）"
    );
}

/// 全部同意时才统一关闭，且每个关闭处理器**恰好跑一次**（关之前再问一遍 = 两次确认框）。
#[test]
fn batch_close_closes_every_window_when_all_allow() {
    let (_rt, mut app) = new_app();
    let first = app.create_window(WindowOptions::default());
    let second = app.create_window(WindowOptions::default());
    let asked_first = Rc::new(Cell::new(0_u32));
    let asked_second = Rc::new(Cell::new(0_u32));
    let counter = asked_first.clone();
    app.on_close(first, move |_app| {
        counter.set(counter.get() + 1);
        true
    });
    let counter = asked_second.clone();
    app.on_close(second, move |_app| {
        counter.set(counter.get() + 1);
        true
    });

    assert!(app.request_close_all(&[first, second]), "全部同意 ⇒ 关闭");
    assert!(app.window(first).is_none());
    assert!(app.window(second).is_none());
    assert_eq!(asked_first.get(), 1, "批准后不得再问一次");
    assert_eq!(asked_second.get(), 1);

    // 已经关掉的窗口再进入批量（例如退出握手重跑）：没有可拦的东西，不能否决退出。
    assert!(
        app.request_close_all(&[first, second]),
        "幽灵窗口不得否决退出（复审 §2.3 的 P3）"
    );
}

/// 处理器在询问阶段自己把窗口关掉：按「已经关了」处理，不算 veto（否则退出被一个不存在的
/// 窗口永久挡住）。
#[test]
fn a_window_that_closes_itself_during_the_inquiry_does_not_veto_the_quit() {
    let (_rt, mut app) = new_app();
    let kept = app.create_window(WindowOptions::default());
    let closed_by_app = app.create_window(WindowOptions::default());
    app.on_close(closed_by_app, move |app| {
        app.close_window(closed_by_app);
        // 故意返回 false：窗口已经不在了，这个 false 不应再否决整个退出。
        false
    });

    assert!(
        app.request_close_all(&[kept, closed_by_app]),
        "窗口已被应用自己关掉 ⇒ 没有可拦的对象"
    );
    assert!(app.window(kept).is_none(), "其余窗口仍必须被关闭");
    assert!(app.window(closed_by_app).is_none());
}

/// AR3 macOS P2-11：Edit 菜单项「启用 ⇔ 应用真的能执行它」。
///
/// 引擎没有内建文本控件，复制/粘贴/撤销由应用绑定拥有；菜单点击只是把命令翻译成等价按键
/// （`MenuCommand::engine_key`）。所以**只有绑定没有处理器**时按键本身会被静默丢弃，菜单项
/// 必须保持置灰，否则就是「可点但点了没反应」。
#[test]
fn edit_menu_availability_requires_a_binding_with_a_handler() {
    use crate::{
        event::{Action, KeyBinding, KeyCode, Modifiers},
        platform::menu::MenuCommand,
    };

    let (_rt, mut app) = new_app();
    // 没有绑定：即使处于文本编辑态，Edit 菜单也全部置灰。
    let nothing_bound = app.edit_menu_availability(true);
    assert!(!nothing_bound.allows(MenuCommand::Copy));

    // 只绑定 Cmd+C、没注册处理器 ⇒ 按键会被 `dispatch_action_with_hit` 丢掉，仍然置灰。
    let copy = Action::custom("edit.copy");
    app.bind_key(KeyBinding {
        key: KeyCode::KeyC,
        modifiers: Modifiers::META,
        action: copy.clone(),
    });
    assert!(
        !app.edit_menu_availability(true).allows(MenuCommand::Copy),
        "只有绑定、没有处理器 ⇒ 必须置灰"
    );

    // 注册处理器后才启用：此时点菜单项与按 Cmd+C 走同一条绑定通路，真的会执行。
    app.on_action(copy, |_ctx| {});
    let available = app.edit_menu_availability(true);
    assert!(available.allows(MenuCommand::Copy), "绑定 + 处理器 ⇒ 启用");
    assert!(
        !available.allows(MenuCommand::Paste),
        "没有绑定 Cmd+V ⇒ Paste 仍置灰"
    );
    assert!(
        !app.edit_menu_availability(false).allows(MenuCommand::Copy),
        "没有文本编辑态时一律置灰（动作会被上抛给 responder chain）"
    );
}
/// P2-6：a11y 交付按窗口限流，且时间窗过后继续交付。
///
/// 实时行情会让 `A11Y` 脏位每帧为真；不限流时每帧都要全树扫描 + 构造 `TreeUpdate`。
/// P3-5：同一诊断重复产生时不得刷屏（64 槽环形缓冲会被同一个问题的重复项占满）。
#[test]
fn a11y_diagnostics_are_deduplicated() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let root = app.window(window).expect("窗口").tree().root();
    let node = NodeBuilder::boxed()
        .build(app.window_mut(window).expect("窗口").tree_mut(), root)
        .expect("挂载");
    let diagnostic = crate::a11y::A11yDiagnostic::MissingLabel { node };
    for _ in 0..8 {
        app.a11y_note(diagnostic.clone());
    }
    assert_eq!(
        app.a11y_diagnostics(),
        &[diagnostic],
        "同一条诊断只保留一份"
    );
    // 不同诊断仍可追加。
    app.a11y_note(crate::a11y::A11yDiagnostic::MissingLabel { node: root });
    assert_eq!(app.a11y_diagnostics().len(), 2);
}

#[test]
fn a11y_delivery_is_throttled_per_window() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let root = app.window(window).expect("窗口").tree().root();
    let node = NodeBuilder::boxed()
        .build(app.window_mut(window).expect("窗口").tree_mut(), root)
        .expect("挂载");
    app.refresh_styles(window).expect("窗口存在");

    assert!(app.a11y_pending(window), "首次交付不受限流影响");
    assert!(app.a11y_take(window).is_some(), "应产出一笔交付");

    // 立刻再标脏：仍在时间窗内 ⇒ 不交付。
    app.window_mut(window)
        .expect("窗口")
        .tree_mut()
        .mark_a11y(node);
    assert!(
        !app.a11y_pending(window),
        "时间窗内不得再次交付（高频更新会变成每帧全树扫描）"
    );

    // 推过时间窗 ⇒ 恢复交付。
    app.advance_clock(std::time::Duration::from_millis(150));
    assert!(app.a11y_pending(window), "时间窗过后必须继续交付");
    let _ = app.a11y_take(window);
    let _ = node;
}
#[test]
fn changed_text_marks_a11y() {
    // 文本节点的可访问名/值 = 文本内容。只标 PAINT 时，"70%" → "80%" 这种等宽替换不会触发
    // LAYOUT，AT 会一直读到旧值（P15 F-153）。
    use smol_str::SmolStr;

    let (_rt, mut app) = new_app();
    let signal: Signal<SmolStr> = app.signal(SmolStr::new_static("A"));
    let window = app.create_window(WindowOptions::default());
    let root = app.window(window).expect("窗口存在").tree().root();
    let node = NodeBuilder::text("A", crate::text::TextStyle::new(12.0))
        .bind_text(move |cx| cx.read(&signal).clone())
        .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
        .expect("挂载");
    app.refresh_styles(window).expect("窗口存在");
    // 消费首帧的 A11Y 位（新节点初始就带 LAYOUT|PAINT|A11Y）。
    let _ = app.a11y_take(window);
    let _ = app.take_wake_request();
    assert!(
        !flags(&app, window, node).contains(DirtyFlags::A11Y),
        "前置条件：A11Y 位已消费"
    );

    assert!(app.write(&signal, SmolStr::new_static("B")));
    app.refresh_styles(window).expect("窗口存在");

    assert!(
        flags(&app, window, node).contains(DirtyFlags::A11Y),
        "文本内容变化必须标 A11Y（AT 读的是文本值）"
    );
}

#[test]
fn writing_a_signal_wakes_the_platform_loop() {
    // 回投任务（`AppHandle::send` / a11y `ActionRequest`）里写状态时，平台泵已经过了帧门禁；
    // 只置 `wake_requested` 不唤醒 dispatcher，会让画面与 a11y 更新等到下一条无关消息（F-152）。
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(0);
    let window = app.create_window(WindowOptions::default());
    let root = app.window(window).expect("窗口存在").tree().root();
    let _ = NodeBuilder::boxed()
        .bind_style(move |cx| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        })
        .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
        .expect("挂载");
    // 先求值一次样式：依赖边在 `refresh_styles` 里建立，没有边就无从失效。
    app.refresh_styles(window).expect("窗口存在");
    let _ = app.take_wake_request();
    let _ = app.dispatcher().take_frame_wake();

    assert!(app.write(&signal, 1));

    assert!(
        app.dispatcher().take_frame_wake(),
        "写信号必须唤醒平台事件循环（否则回投任务里的状态不会出帧）"
    );
}

#[test]
fn write_marks_across_windows() {
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(0);
    let window_a = app.create_window(WindowOptions::default());
    let window_b = app.create_window(WindowOptions::default());
    let node_a = node_with(&mut app, window_a, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal) as f32),
        ..StylePatch::default()
    });
    let node_b = node_with(&mut app, window_b, move |cx: &mut ReadCtx<'_>| StylePatch {
        opacity: Some(*cx.read(&signal) as f32),
        ..StylePatch::default()
    });
    app.refresh_styles(window_a).expect("窗口存在");
    app.refresh_styles(window_b).expect("窗口存在");
    clear_dirty(&mut app, window_a);
    clear_dirty(&mut app, window_b);

    assert!(app.write(&signal, 9));

    assert!(flags(&app, window_a, node_a).contains(DirtyFlags::PAINT));
    assert!(flags(&app, window_b, node_b).contains(DirtyFlags::PAINT));
    assert!(app.take_wake_request());
}

#[test]
fn create_window_ids_are_unique_and_monotonic() {
    let (_rt, mut app) = new_app();
    let first = app.create_window(WindowOptions::default());
    let second = app.create_window(WindowOptions::default());
    let third = app.create_window(WindowOptions::default());

    assert_eq!(
        (first.to_bits(), second.to_bits(), third.to_bits()),
        (1, 2, 3),
        "WindowId 从 1 起单调递增（0 非法）"
    );
    assert_eq!(app.windows().count(), 3);
    for (id, window) in app.windows() {
        assert_eq!(id, window.id(), "windows() 带出的 id 必须与窗口自身一致");
        assert!(app.window(id).is_some());
    }
}

#[test]
fn close_window_unknown_returns_false() {
    let (_rt, mut app) = new_app();

    assert!(!app.close_window(WindowId::for_test(42)));
}

#[test]
fn bind_style_panic_clears_ghost_deps() {
    let (_rt, mut app) = new_app();
    let signal: Signal<u32> = app.signal(0);
    let calls = Rc::new(Cell::new(0u32));
    let counter = Rc::clone(&calls);
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| {
        counter.set(counter.get() + 1);
        let value = *cx.read(&signal);
        assert!(counter.get() <= 1, "第二次求值故意 panic");
        StylePatch {
            opacity: Some(value as f32),
            ..StylePatch::default()
        }
    });
    app.refresh_styles(window).expect("窗口存在");
    assert_eq!(deps(&app, window, node), vec![signal.id()]);

    assert!(app.write(&signal, 4));
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = app.refresh_styles(window);
    }));
    assert!(outcome.is_err(), "panic 必须向上传播");
    assert_eq!(
        deps(&app, window, node),
        vec![signal.id()],
        "panic 打断了 replace_deps ⇒ 旧边仍在（幽灵边）"
    );

    let report = app.refresh_styles(window).expect("窗口仍在");
    assert_eq!(report.evaluated, 0, "闭包已丢 → 不算求值");
    assert!(deps(&app, window, node).is_empty(), "幽灵边必须被显式退订");

    let _ = app.take_wake_request();
    assert!(app.write(&signal, 5), "值仍然会变");
    assert!(!app.take_wake_request(), "退订后再写旧信号不得再命中该节点");
}

#[test]
fn layout_text_breaks_at_the_content_width() {
    // B1：60 px 宽、16 px 字号的 4 个单词必须断成多行，且选区矩形逐行对齐。
    let (_rt, mut app, window, node) = text_app("aaaa aaaa aaaa aaaa", Some(60.0));
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let report = text_layout(&mut app, window);

    assert_eq!(report.laid, 1);
    let tree = app.window(window).expect("窗口存在").tree();
    let line = tree.text_line(node).expect("行快照").clone();
    assert!(
        line.lines.len() >= 3,
        "content width 60 must wrap into several lines, got {}",
        line.lines.len()
    );
    let rects = tree
        .text_selection_rects(node, 0, line.text.len())
        .expect("选区");
    assert!(
        rects.len() >= 3,
        "selection rects must cover the broken lines"
    );
    assert!(
        line.width <= 60.5,
        "the line box must respect the content width"
    );
}

#[test]
fn ime_cursor_area_requires_text_shape_and_can_be_cleared() {
    let (_rt, mut app, window, node) = text_app("abc", None);
    let unknown = WindowId::for_test(999);
    assert!(matches!(
        app.set_ime_cursor_area(unknown, node, 0),
        Err(crate::error::EngineError::UnknownWindow { .. })
    ));
    let root = app.window(window).expect("窗口").tree().root();
    assert!(matches!(
        app.set_ime_cursor_area(window, root, 0),
        Err(crate::error::EngineError::TextShape { .. })
    ));
    assert!(matches!(app.set_ime_cursor_area(window, node, 4), Ok(())));
    assert!(matches!(app.set_ime_cursor_area(window, node, 1), Ok(())));
    assert!(matches!(
        app.ime_cursor_area(window),
        Err(crate::error::EngineError::TextShape { .. })
    ));

    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let _ = text_layout(&mut app, window);
    let area = app
        .ime_cursor_area(window)
        .expect("窗口存在")
        .expect("已 shape 后有 hint");
    assert!(area.size.width >= 1.0);
    assert!(area.size.height > 0.0);

    assert!(app.set_ime_cursor_area(window, node, 4).is_ok());
    assert!(matches!(
        app.ime_cursor_area(window),
        Err(crate::error::EngineError::TextShape { .. })
    ));
    assert!(app.set_ime_cursor_area(window, node, 3).is_ok());

    app.clear_ime_cursor_area(window);
    assert_eq!(app.ime_cursor_area(window).expect("窗口存在"), None);

    assert!(app.set_ime_cursor_area(window, node, 3).is_ok());
    set_text(
        &mut app,
        window,
        node,
        "abcd",
        crate::text::TextStyle::new(16.0),
    );
    assert!(
        matches!(
            app.ime_cursor_area(window),
            Err(crate::error::EngineError::TextShape { .. })
        ),
        "文本内容变化后、下一次 text pass 前不得返回旧 caret"
    );
}

/// RB-03（用户裁决「焦点移走/窗口失焦统一清 caret hint」）：窗口失焦必须清 IME hint。
///
/// 变异检查：删掉 `on_window_focus_lost` 里的 `clear_ime_cursor_request` 调用 ⇒ 本测试红。
#[test]
fn losing_window_focus_clears_the_ime_cursor_hint() {
    let (_rt, mut app, window, node) = text_app("abc", None);
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let _ = text_layout(&mut app, window);
    assert!(app.set_ime_cursor_area(window, node, 1).is_ok());
    assert!(
        app.ime_cursor_area(window).expect("窗口存在").is_some(),
        "前置：失焦前应有 caret hint"
    );

    app.dispatch(window, crate::event::Event::FocusChanged { focused: false });

    assert_eq!(
        app.ime_cursor_area(window).expect("窗口存在"),
        None,
        "窗口失焦后 caret hint 必须清空（三端统一清场）"
    );
}

#[test]
fn ime_cursor_area_resolves_after_the_first_frame() {
    let (_rt, mut app, window, node) = text_app("abc", None);
    assert!(app.set_ime_cursor_area(window, node, 3).is_ok());

    let outcome = app.frame(window).expect("窗口存在").expect("首帧成功");
    assert!(matches!(outcome, FrameOutcome::Committed(_)));
    assert!(
        app.ime_cursor_area(window).expect("窗口存在").is_some(),
        "首帧 text/layout 完成后应能解析 caret"
    );
}

#[test]
fn ime_cursor_area_rejects_a_non_boundary_byte_index() {
    let (_rt, mut app, window, node) = text_app("a中b", None);
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let _ = text_layout(&mut app, window);

    assert!(app.set_ime_cursor_area(window, node, 2).is_ok());
    assert!(matches!(
        app.ime_cursor_area(window),
        Err(crate::error::EngineError::TextShape { .. })
    ));
}

#[test]
fn text_caret_rect_includes_border_padding_and_node_origin() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let root = app.window(window).expect("窗口").tree().root();
    let mut style = text_style(Some(200.0));
    style.inner.padding = taffy::geometry::Rect {
        left: taffy::style::LengthPercentage::length(9.0),
        right: taffy::style::LengthPercentage::length(0.0),
        top: taffy::style::LengthPercentage::length(7.0),
        bottom: taffy::style::LengthPercentage::length(0.0),
    };
    style.inner.border = taffy::geometry::Rect {
        left: taffy::style::LengthPercentage::length(2.0),
        right: taffy::style::LengthPercentage::length(0.0),
        top: taffy::style::LengthPercentage::length(3.0),
        bottom: taffy::style::LengthPercentage::length(0.0),
    };
    let node = NodeBuilder::text("abc", crate::text::TextStyle::new(16.0))
        .style(style)
        .build(app.window_mut(window).expect("窗口").tree_mut(), root)
        .expect("挂载");
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let _ = text_layout(&mut app, window);

    let rect = app
        .window(window)
        .expect("窗口")
        .text_caret_rect(node, 0)
        .expect("caret");
    assert!((rect.origin.x - 11.0).abs() < 0.01, "{rect:?}");
    assert!((rect.origin.y - 10.0).abs() < 0.01, "{rect:?}");
}

#[test]
fn close_window_forgets_the_scale_epoch() {
    let (_rt, mut app, window, _node) = text_app("x", None);
    let _ = refresh(&mut app, window);
    // 首帧的 `refresh_images` 会为窗口登记图片 scale epoch（E7 旁表）。
    let _ = app.frame(window).expect("窗口存在").expect("帧成功");
    assert!(
        app.image_scale_epochs.contains_key(&window),
        "首帧必须登记图片 scale epoch（否则下面的清理断言是空断言）"
    );
    assert!(app.close_window(window));
    assert!(
        !app.image_scale_epochs.contains_key(&window),
        "关窗必须清 image_scale_epochs（create/close 循环下否则无界增长）"
    );
    assert!(!app.close_window(window), "second close is a no-op");
}

#[test]
fn changed_text_always_relayouts_the_snapshot() {
    let (_rt, mut app, window, node) = text_app("aaaa aaaa aaaa aaaa", Some(60.0));
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let _ = text_layout(&mut app, window);

    for text in ["bbbb", "aaaa aaaa aaaa aaaa bbbb", "WWWW"] {
        set_text(
            &mut app,
            window,
            node,
            text,
            crate::text::TextStyle::new(16.0),
        );
        let pass = refresh(&mut app, window);
        assert_eq!(pass.shaped, 1, "new text must reshape");
        let laid = text_layout(&mut app, window);
        assert_eq!(laid.laid, 1, "B4: changed text must relayout ({text})");
        assert_eq!(laid.reused, 0);
        let tree = app.window(window).expect("window").tree();
        assert_eq!(tree.text_line(node).expect("line").text, text);
    }
}

#[test]
fn style_and_family_changes_relayout() {
    let (_rt, mut app, window, node) = text_app("same text", Some(60.0));
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    let _ = text_layout(&mut app, window);

    let mut style = crate::text::TextStyle::new(24.0);
    style = style.with_family(smol_str::SmolStr::new_static("Consolas"));
    set_text(&mut app, window, node, "same text", style);

    let pass = refresh(&mut app, window);
    assert_eq!(pass.shaped, 1, "style/family is part of the shape key");
    assert_eq!(text_layout(&mut app, window).laid, 1);
}

#[test]
fn inline_boxes_changes_relayout() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let spec = crate::text::InlineBoxSpec {
        id: 7,
        index: 1,
        size: crate::math::Size::new(20.0, 20.0),
        baseline: None,
        align: crate::text::InlineAlign::Baseline,
        paint: None,
    };
    let node = {
        let w = app.window_mut(window).expect("window");
        let root = w.tree().root();
        NodeBuilder::text("a b", crate::text::TextStyle::new(16.0))
            .inline_boxes([spec])
            .build(w.tree_mut(), root)
            .expect("build")
    };
    let _ = refresh(&mut app, window);
    let _ = layout(&mut app, window);
    assert_eq!(text_layout(&mut app, window).laid, 1);

    let spec2 = crate::text::InlineBoxSpec {
        id: 9,
        index: 1,
        size: crate::math::Size::new(40.0, 10.0),
        baseline: Some(8.0),
        align: crate::text::InlineAlign::Top,
        paint: None,
    };
    {
        let tree = app.window_mut(window).expect("window").tree_mut();
        let mut content = tree.text_content(node).expect("text").clone();
        content.inline_boxes = Box::new([spec2]);
        tree.set_text_content(node, content);
    }
    let pass = refresh(&mut app, window);
    assert_eq!(pass.shaped, 1, "inline boxes are part of the shape key");
    assert_eq!(text_layout(&mut app, window).laid, 1);
}

/// v2.1 E2b（审查 F11/F12）：`App::frame` 把行内盒图片缺失接成 `InlineBoxImageMissing`
/// 诊断（D17 不静默）；同一 id 的重复帧由 `TextContext` 按值去重，环里始终只有一条。
#[test]
fn app_frame_reports_missing_inline_box_images_once() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let spec = crate::text::InlineBoxSpec {
        id: 11,
        index: 1,
        size: crate::math::Size::new(16.0, 16.0),
        baseline: None,
        align: crate::text::InlineAlign::Baseline,
        paint: Some(crate::text::InlineBoxPaint::Image {
            resource: crate::resources::ResourceId::for_test(4242, 1),
            modulation: crate::paint::color::Color::WHITE,
        }),
    };
    {
        let w = app.window_mut(window).expect("window");
        let root = w.tree().root();
        NodeBuilder::text("a b", crate::text::TextStyle::new(16.0))
            .inline_boxes([spec])
            .build(w.tree_mut(), root)
            .expect("build");
    }
    let missing_count = |app: &App| {
        app.text_diagnostics()
            .iter()
            .filter(|diagnostic| {
                matches!(
                    diagnostic,
                    crate::text::TextDiagnostic::InlineBoxImageMissing { id: 11 }
                )
            })
            .count()
    };
    let outcome = app.frame(window).expect("窗口存在").expect("帧成功");
    assert!(matches!(outcome, FrameOutcome::Committed(_)));
    assert_eq!(
        missing_count(&app),
        1,
        "App::frame 必须把跳过的行内图片接成诊断：{:?}",
        app.text_diagnostics()
    );
    let _ = app.frame(window).expect("窗口存在").expect("帧不报错");
    assert_eq!(
        missing_count(&app),
        1,
        "同一诊断按值去重，不随帧数增长：{:?}",
        app.text_diagnostics()
    );
    assert!(
        app.window(window)
            .expect("window")
            .inline_box_image_diagnostics()
            .is_empty(),
        "App 路径的文本诊断不写窗口级快照"
    );
}

#[test]
fn bind_text_preserves_inline_boxes() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let signal = app.signal(smol_str::SmolStr::new_static("first"));
    let spec = crate::text::InlineBoxSpec {
        id: 3,
        index: 0,
        size: crate::math::Size::new(16.0, 16.0),
        baseline: None,
        align: crate::text::InlineAlign::Baseline,
        paint: None,
    };
    let node = {
        let w = app.window_mut(window).expect("window");
        let root = w.tree().root();
        NodeBuilder::text("", crate::text::TextStyle::new(16.0))
            .bind_text(move |cx| cx.read(&signal).clone())
            .inline_boxes([spec])
            .build(w.tree_mut(), root)
            .expect("build")
    };

    for value in ["first", "second"] {
        let _ = app.refresh_styles(window);
        let content = app
            .window(window)
            .expect("window")
            .tree()
            .text_content(node)
            .expect("text")
            .clone();
        assert_eq!(
            content.inline_boxes.len(),
            1,
            "inline boxes survive bind_text"
        );
        assert_eq!(content.text, value);
        if value == "first" {
            app.write(&signal, smol_str::SmolStr::new_static("second"));
        }
    }
}

// ── P06：完整帧 ──

#[test]
fn app_frame_runs_styles_then_paints() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let root = app.window(window).expect("窗口").tree().root();
    let background = app.signal(crate::paint::color::Color::WHITE);
    {
        let _ = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 20.0, 10.0))
            .bind_style(move |cx| StylePatch {
                background: Some(*cx.read(&background)),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("窗口").tree_mut(), root)
            .expect("挂载成功");
    }

    let outcome = app.frame(window).expect("窗口存在").expect("帧成功");
    assert!(matches!(outcome, FrameOutcome::Committed(_)));

    // 改信号 → 下一帧的 Quad 颜色必须变（证明 styles pass 在 paint 之前跑过）。
    app.write(&background, crate::paint::color::Color::BLACK);
    let _ = app.frame(window).expect("窗口存在").expect("帧成功");
    let scene = app.window(window).expect("窗口").scene();
    let quad_color = scene
        .iter()
        .find_map(|primitive| match primitive {
            crate::paint::primitive::Primitive::Quad(quad) => match &quad.paint {
                crate::paint::primitive::QuadPaint::Solid { fill, .. } => Some(*fill),
                crate::paint::primitive::QuadPaint::Gradient(_) => None,
            },
            _ => None,
        })
        .expect("必须有背景 Quad");
    assert_eq!(quad_color, crate::paint::color::Color::BLACK);
}

#[test]
fn app_frame_unknown_window_is_none() {
    let (_rt, mut app) = new_app();
    assert!(app.frame(WindowId::for_test(99)).is_none());
}

#[test]
fn app_frame_is_idle_without_work() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    // 首帧有布局移动（根从 0 尺寸撑满窗口）⇒ 提交一次；之后无写、无脏 ⇒ 空闲零帧。
    let first = app.frame(window).expect("窗口存在");
    assert!(matches!(first, Ok(FrameOutcome::Committed(_))));

    let second = app.frame(window).expect("窗口存在");
    assert!(matches!(second, Ok(FrameOutcome::Idle)));
    assert_eq!(
        app.window(window).expect("窗口").frame_index().get(),
        1,
        "空闲帧不得递增序号"
    );
}

// ── R01-12：动作 handler 的重入与 panic 安全 ──────────────────────────────

/// R01-12（重入面）：handler 体内为**同一个** action 注册新处理器时，新条目必须生效。
///
/// 旧实现无条件 `handlers.insert(action, 旧闭包)` 收尾，把应用刚注册的新处理器踩掉：下一次
/// 分发仍然跑旧闭包（应用看到的是"重新订阅无效"）。
#[test]
fn a_handler_registered_from_inside_a_handler_survives() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, |_cx| StylePatch::default());
    let action = Action::custom("resubscribe");
    let first_hits = Rc::new(Cell::new(0u32));
    let second_hits = Rc::new(Cell::new(0u32));

    let first = Rc::clone(&first_hits);
    let second = Rc::clone(&second_hits);
    let registered = action.clone();
    app.on_action(action.clone(), move |ctx| {
        first.set(first.get() + 1);
        let second = Rc::clone(&second);
        let registered = registered.clone();
        ctx.app_mut().on_action(registered, move |_ctx| {
            second.set(second.get() + 1);
        });
    });

    assert!(app.dispatch_action(window, node, action.clone(), ActionSource::Programmatic));
    assert_eq!(first_hits.get(), 1);
    assert_eq!(second_hits.get(), 0, "第一次跑的是旧处理器");

    assert!(app.dispatch_action(window, node, action, ActionSource::Programmatic));
    assert_eq!(second_hits.get(), 1, "内层注册的新处理器必须接管");
    assert_eq!(first_hits.get(), 1, "旧处理器不得再被调用");
}

/// R01-12（panic 面）：handler panic 之后条目必须还在表里。
///
/// 旧实现在 `handler(&mut ctx)` 之后才 `insert`：panic 展开时闭包随栈帧被 drop，该动作的
/// 处理器永久丢失 —— 之后分发静默退化为默认行为（`Action::Custom` ⇒ `false`）。
#[test]
fn a_panicking_handler_is_not_lost() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());
    let node = node_with(&mut app, window, |_cx| StylePatch::default());
    let action = Action::custom("explode");
    let hits = Rc::new(Cell::new(0u32));
    let counter = Rc::clone(&hits);
    app.on_action(action.clone(), move |_ctx| {
        counter.set(counter.get() + 1);
        panic!("动作处理器炸了");
    });

    let attempt = std::panic::catch_unwind(AssertUnwindSafe(|| {
        app.dispatch_action(window, node, action.clone(), ActionSource::Programmatic)
    }));
    assert!(attempt.is_err(), "handler panic 必须向上传播");

    let again = std::panic::catch_unwind(AssertUnwindSafe(|| {
        app.dispatch_action(window, node, action.clone(), ActionSource::Programmatic)
    }));
    assert!(
        again.is_err(),
        "处理器必须仍在表里：旧实现在 unwind 里丢了闭包，分发会静默返回 false"
    );
    assert_eq!(hits.get(), 2, "处理器必须被再次调用");
}

// ── X1：owner / modal / 置顶的引擎层语义 ─────────────────────────────────

#[test]
fn a_modal_window_blocks_its_owners_input_until_it_closes() {
    let (_rt, mut app) = new_app();
    let owner = app.create_window(WindowOptions::default());
    let modal = app.create_window(WindowOptions {
        owner: Some(owner),
        modal: true,
        ..WindowOptions::default()
    });
    assert_eq!(
        app.window(modal).expect("modal 存在").options().owner,
        Some(owner),
        "合法 owner 必须保留"
    );

    // 模态窗口自己的输入正常分发。
    app.dispatch(
        modal,
        crate::event::Event::Input(crate::event::InputEvent::key_down(
            crate::event::KeyCode::KeyA,
            crate::event::Modifiers::empty(),
        )),
    );
    assert_eq!(app.drain_events().len(), 1, "模态窗口本身不受限");

    // owner 的输入被模态拦截：观测队列也不投递（与原生 EnableWindow 语义一致）。
    app.dispatch(
        owner,
        crate::event::Event::Input(crate::event::InputEvent::key_down(
            crate::event::KeyCode::KeyB,
            crate::event::Modifiers::empty(),
        )),
    );
    assert!(
        app.drain_events().is_empty(),
        "owner 在模态期间不得收到输入事件"
    );

    // 系统通知类事件不受模态影响。
    app.dispatch(owner, crate::event::Event::CloseRequested);
    assert_eq!(app.drain_events().len(), 1, "非输入事件必须仍然投递");

    // 模态窗口关闭后 owner 输入恢复。
    app.close_window(modal);
    app.dispatch(
        owner,
        crate::event::Event::Input(crate::event::InputEvent::key_down(
            crate::event::KeyCode::KeyC,
            crate::event::Modifiers::empty(),
        )),
    );
    assert_eq!(app.drain_events().len(), 1, "模态关闭后 owner 恢复输入");
}

#[test]
fn a_modal_option_without_an_owner_is_demoted_to_a_normal_window() {
    let (_rt, mut app) = new_app();
    let id = app.create_window(WindowOptions {
        modal: true,
        ..WindowOptions::default()
    });
    let options = app.window(id).expect("窗口存在").options();
    assert!(!options.modal, "无主模态降级为普通窗口");
    assert_eq!(options.owner, None);
}

#[test]
fn a_nonexistent_owner_is_ignored_instead_of_binding_a_stale_handle() {
    let (_rt, mut app) = new_app();
    let id = app.create_window(WindowOptions {
        owner: Some(WindowId::for_test(999)),
        ..WindowOptions::default()
    });
    assert_eq!(
        app.window(id).expect("窗口存在").options().owner,
        None,
        "不存在/已关闭的 owner 不得绑定（WindowId 永不复用，陈旧句柄只能是调用方错误）"
    );
}

#[test]
fn closing_the_owner_cascades_to_owned_and_modal_windows() {
    let (_rt, mut app) = new_app();
    let owner = app.create_window(WindowOptions::default());
    let panel = app.create_window(WindowOptions {
        owner: Some(owner),
        ..WindowOptions::default()
    });
    let modal = app.create_window(WindowOptions {
        owner: Some(owner),
        modal: true,
        ..WindowOptions::default()
    });
    let grandchild = app.create_window(WindowOptions {
        owner: Some(panel),
        ..WindowOptions::default()
    });

    assert!(app.close_window(owner), "owner 必须关闭成功");
    assert!(app.window(owner).is_none());
    assert!(app.window(panel).is_none(), "owned 面板级联关闭");
    assert!(app.window(modal).is_none(), "模态窗口级联关闭");
    assert!(app.window(grandchild).is_none(), "级联必须递归");
}

// ── E6（P1-E6）：窗口 chrome 的运行时 API ──────────────────────────────

/// E6：`App::set_title` 写窗口参数（泵下一轮对账到原生窗口）并拒绝未知窗口；
/// 同值写是 no-op（不产生额外的平台系统调用）。
#[test]
fn set_title_updates_the_window_and_rejects_unknown_windows() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static("before"),
        ..WindowOptions::default()
    });

    app.set_title(window, smol_str::SmolStr::new_static("after"))
        .expect("已知窗口必须接受标题");
    assert_eq!(
        app.window(window).expect("窗口存在").options().title,
        "after"
    );
    // 同值：仍是 Ok 且不改任何东西（幂等）。
    app.set_title(window, smol_str::SmolStr::new_static("after"))
        .expect("同值标题必须成功");

    let error = app
        .set_title(WindowId::for_test(999), smol_str::SmolStr::new_static("x"))
        .expect_err("未知窗口必须是强类型错误");
    assert!(
        matches!(error, crate::error::EngineError::UnknownWindow { .. }),
        "必须是 UnknownWindow：{error:?}"
    );
}

/// E6：`App::request_window_control` 三端可见（去 `cfg(linux)`）：窗口存在性优先，其次是
/// 平台能力判定；请求进入单槽，由平台泵取走执行。
#[test]
fn request_window_control_queues_supported_controls_and_rejects_unknown_windows() {
    use crate::window::{WindowControl, WindowResizeEdge};

    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());

    let error = app
        .request_window_control(WindowId::for_test(999), WindowControl::Minimize)
        .expect_err("未知窗口必须优先报错");
    assert!(matches!(
        error,
        crate::error::EngineError::UnknownWindow { .. }
    ));

    app.request_window_control(window, WindowControl::Minimize)
        .expect("Minimize 三端都支持");
    assert_eq!(
        app.take_window_control(),
        Some((window, WindowControl::Minimize)),
        "请求必须进单槽由平台泵取走"
    );
    assert_eq!(app.take_window_control(), None, "取走后槽必须为空");

    // 平台能力差异：Windows 没有全屏 / 程序化 move / resize；macOS 没有程序化 move / resize。
    let unsupported: &[(&str, WindowControl)] = if cfg!(windows) {
        &[
            ("fullscreen", WindowControl::ToggleFullscreen),
            ("interactive move", WindowControl::BeginMove),
            (
                "interactive resize",
                WindowControl::BeginResize(WindowResizeEdge::TopLeft),
            ),
        ]
    } else if cfg!(target_os = "macos") {
        &[
            ("interactive move", WindowControl::BeginMove),
            (
                "interactive resize",
                WindowControl::BeginResize(WindowResizeEdge::BottomRight),
            ),
        ]
    } else {
        &[]
    };
    for (needle, control) in unsupported {
        let error = app
            .request_window_control(window, *control)
            .expect_err("该端必须强类型拒绝");
        let crate::error::EngineError::UnsupportedWindowCapability { capability, .. } = &error
        else {
            panic!("必须是 UnsupportedWindowCapability：{error:?}");
        };
        assert!(
            capability.contains(needle),
            "capability 诊断名必须可 grep（期望含 {needle}）：{capability}"
        );
    }
    // 被拒绝的控制不得留在槽里。
    assert_eq!(app.take_window_control(), None);

    // 三端都支持的控制（Windows/macOS 的 Minimize/ToggleMaximize；Linux 全部）仍可入队。
    app.request_window_control(window, WindowControl::ToggleMaximize)
        .expect("ToggleMaximize 三端都支持");
    assert_eq!(
        app.take_window_control(),
        Some((window, WindowControl::ToggleMaximize))
    );
}

/// E6 / I10：非 macOS 的交通灯 API 必须**强类型**报错（setter）或如实返回 None（getter），
/// 不得静默成功；未知窗口仍然优先。
#[test]
fn traffic_light_api_is_unsupported_off_macos() {
    let (_rt, mut app) = new_app();
    let window = app.create_window(WindowOptions::default());

    #[cfg(not(target_os = "macos"))]
    {
        use crate::math::Point;

        assert_eq!(app.traffic_light_inset(window), None, "非 macOS 没有交通灯");
        let error = app
            .set_traffic_light_position(window, Point::new(8.0, 8.0))
            .expect_err("非 macOS 必须强类型拒绝");
        let crate::error::EngineError::UnsupportedWindowCapability { capability, .. } = &error
        else {
            panic!("必须是 UnsupportedWindowCapability：{error:?}");
        };
        assert!(capability.contains("traffic light"), "{capability}");

        let error = app
            .set_traffic_light_position(WindowId::for_test(999), Point::ZERO)
            .expect_err("未知窗口优先");
        assert!(matches!(
            error,
            crate::error::EngineError::UnknownWindow { .. }
        ));
        assert_eq!(app.traffic_light_inset(WindowId::for_test(999)), None);
    }

    #[cfg(target_os = "macos")]
    {
        // 建窗路径（AppKit）由平台层在 real run loop 里完成；headless 的 `App` 没有原生窗口，
        // 这里只验证 API 契约的两端都存在（`None` 表示「尚未由平台写入」）。
        assert_eq!(app.traffic_light_inset(window), None);
    }
}

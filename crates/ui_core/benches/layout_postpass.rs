//! wave-6 状态/布局/命中热路径基准（criterion；**非合并门禁**）。
//!
//! 三项对应审查报告里"状态写入 / 布局后置 / 命中测试不得回退"的行为面：
//!
//! - `state_write_*`：`App::write`（等值短路 → 失效传播 → 唤醒位）的固定成本；
//! - `layout_single_change_10k`：10k 节点树里改**一个**叶子后的增量布局（taffy cache +
//!   wave-6 的增量子树包络维护，B9）；
//! - `hit_test_grid_10k`：10k 节点网格（100 行 × 100 列）中心点的命中测试（子树包络剪枝）。
//!
//! **可复现性**：headless（无 renderer）、输入全字面量、fixture 在 `iter_batched` 的 setup 段
//! （**不计时**）。采样 50 samples / 1 s warmup / 3 s 测量，与 `perf.rs` 同口径。
//!
//! 本文件**同时**能在 wave-6 之前的分支上编译（只用两版都存在的公开 API），
//! 因此可以直接对比"整改前/后"的数值（见整改报告的基准表）。

use std::{hint::black_box, time::Duration};

use criterion::{Criterion, criterion_group, criterion_main};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    math::{Point, Scale, Size},
    node::{builder::NodeBuilder, id::NodeId},
    paint::style::StylePatch,
    window::{WindowId, WindowOptions},
};

/// 逻辑视口（固定，避免窗口尺寸影响布局/命中成本）。
const VIEWPORT: Size = Size::new(1000.0, 1000.0);

/// 网格：100 行 × 100 列 = 10k 个 8×8 节点。
const GRID_SIDE: usize = 100;
const CELL: f32 = 8.0;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
}

fn fixture(handle: &tokio::runtime::Handle) -> (App, WindowId) {
    let mut app = App::new(handle.clone());
    let window = app.create_window(WindowOptions {
        title: SmolStr::new_static("bench"),
        size: VIEWPORT,
        scale: Scale::ONE,
        ..WindowOptions::default()
    });
    (app, window)
}

fn fixed(width: f32, height: f32) -> ui_core::paint::style::Style {
    let mut inner = taffy::Style::<SmolStr>::DEFAULT;
    inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(width),
        height: taffy::style::Dimension::length(height),
    };
    ui_core::paint::style::Style {
        inner,
        clip: ui_core::paint::style::Clip::None,
    }
}

/// 建 100 行 × 100 列的网格（行是 column flex，列是 row flex）。
fn mount_grid(app: &mut App, window: WindowId) -> Vec<NodeId> {
    let root = app.window(window).expect("window").tree().root();
    let mut leaves = Vec::with_capacity(GRID_SIDE * GRID_SIDE);
    for _ in 0..GRID_SIDE {
        let row = NodeBuilder::boxed()
            .style(fixed(GRID_SIDE as f32 * CELL, CELL))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("row mount");
        for _ in 0..GRID_SIDE {
            let leaf = NodeBuilder::boxed()
                .style(fixed(CELL, CELL))
                .build(app.window_mut(window).expect("window").tree_mut(), row)
                .expect("leaf mount");
            leaves.push(leaf);
        }
    }
    let _ = app.window_mut(window).expect("window").compute_layout();
    leaves
}

fn benches(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("wave6");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // ── 状态写入：无读者（最短路径：等值短路失败 → store 写 → 反查表空） ──
    {
        let (mut app, _window) = fixture(rt.handle());
        let signal: ui_core::node::signal::Signal<u32> = app.signal(0);
        let mut next = 0u32;
        group.bench_function("state_write_no_readers", |b| {
            b.iter(|| {
                next = next.wrapping_add(1);
                black_box(app.write(&signal, black_box(next)));
            });
        });
    }

    // ── 状态写入：一个读者（失效传播 → 标脏 → 唤醒位） ──
    {
        let (mut app, window) = fixture(rt.handle());
        let signal: ui_core::node::signal::Signal<SmolStr> = app.signal(SmolStr::new_static("0"));
        let root = app.window(window).expect("window").tree().root();
        let _node = NodeBuilder::boxed()
            .bind_style(move |cx| StylePatch {
                // `clone().len()` 而不是 `*read(...)`：让同一份基准源码在 wave-6 前后都能编译
                // （旧 API 返回 T，新 API 返回 `SignalRef<T>`，两者都能 `.clone()`）。
                opacity: Some(cx.read(&signal).clone().len() as f32),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let mut next = 0usize;
        group.bench_function("state_write_one_reader", |b| {
            b.iter(|| {
                next += 1;
                let value = format_smolstr!("{next}");
                black_box(app.write(&signal, black_box(value)));
            });
        });
    }

    // ── 布局：10k 节点网格里改一个叶子的高度（不推挤任何兄弟） ──
    {
        let (mut app, window) = fixture(rt.handle());
        let leaves = mount_grid(&mut app, window);
        let last = *leaves.last().expect("non-empty");
        let mut tall = false;
        group.bench_function("layout_single_change_10k", |b| {
            b.iter(|| {
                tall = !tall;
                let height = if tall { CELL * 2.0 } else { CELL };
                app.window_mut(window)
                    .expect("window")
                    .tree_mut()
                    .set_style(last, fixed(CELL, height))
                    .expect("leaf alive");
                black_box(
                    app.window_mut(window)
                        .expect("window")
                        .compute_layout()
                        .expect("layout ok"),
                );
            });
        });
    }

    // ── 命中：10k 节点网格的中心点（子树包络剪枝） ──
    {
        let (mut app, window) = fixture(rt.handle());
        let _ = mount_grid(&mut app, window);
        let center = Point::new(GRID_SIDE as f32 * CELL / 2.0, GRID_SIDE as f32 * CELL / 2.0);
        group.bench_function("hit_test_grid_10k", |b| {
            let tree = app.window(window).expect("window").tree();
            b.iter(|| black_box(tree.hit_test(black_box(center))));
        });
    }

    group.finish();
}

criterion_group!(layout_postpass, benches);
criterion_main!(layout_postpass);

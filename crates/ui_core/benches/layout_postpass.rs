use std::{hint::black_box, time::Duration};

use criterion::{Criterion, criterion_group, criterion_main};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    math::{DevicePixelRatio, LogicalPoint, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    window::{WindowId, WindowOptions},
};

const VIEWPORT: LogicalSize = LogicalSize::new(px(1000.0), px(1000.0));

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
        scale: DevicePixelRatio::ONE,
        ..WindowOptions::default()
    });
    (app, window)
}

fn fixed(width: f32, height: f32) -> ui_core::paint::style::Style {
    StyleBuilder::new().size(px(width), px(height)).build()
}

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

    {
        let (mut app, window) = fixture(rt.handle());
        let signal: ui_core::node::signal::Signal<SmolStr> = app.signal(SmolStr::new_static("0"));
        let root = app.window(window).expect("window").tree().root();
        let _node = NodeBuilder::boxed()
            .bind_style(move |cx| StylePatch {
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

    {
        let (mut app, window) = fixture(rt.handle());
        let _ = mount_grid(&mut app, window);
        let center = LogicalPoint::new(
            px(GRID_SIDE as f32 * CELL / 2.0),
            px(GRID_SIDE as f32 * CELL / 2.0),
        );
        group.bench_function("hit_test_grid_10k", |b| {
            let tree = app.window(window).expect("window").tree();
            b.iter(|| black_box(tree.hit_test(black_box(center))));
        });
    }

    group.finish();
}

criterion_group!(layout_postpass, benches);
criterion_main!(layout_postpass);

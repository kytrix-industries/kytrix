//! L4 性能基准（criterion；**非合并门禁**，只要求 `cargo bench --no-run` 可编译）。
//!
//! 覆盖主设计 §6.7 的六项：颜色转换 / 塑形 / 布局 / 实例构建 / damage 合并 / 依赖失效。
//! 后两项没有独立的公开入口（`DamageAccumulator` 与 `FrameEncoder` 是 crate 私有，
//! 不应为 bench 扩公开面），因此由 `frame_*` 组顺带测量——见
//! `design/phase-13-resources-snapshot.md` §10.2。
//!
//! 可复现性：headless（`renderer = None`）⇒ 不受窗口前台化 / 组合器影响；输入全字面量；
//! 每组在 setup 里建 `current_thread` runtime + `App`（**不计时**，`iter_batched` 的 setup 段）。
//! 采样：50 samples / 1 s warmup / 3 s 测量——`frame_*` 单次 ~1 ms、setup ~2 ms，50 是
//! 「统计稳定」与「总时长 < 5 min」的折中；数值**不进 CI 判定**（D19）。

use std::{hint::black_box, time::Duration};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use kytrix_gui::{
    app::App,
    math::{Rect, Scale, Size},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        color::{Color, ColorResolver, Interpolation},
        style::{PaintSpec, StylePatch},
    },
    text::TextStyle,
    window::{WindowId, WindowOptions},
};
use smol_str::SmolStr;

/// 逻辑视口（固定，避免窗口尺寸影响布局成本）。
const VIEWPORT: Size = Size::new(800.0, 600.0);

/// 一次基准的夹具：runtime 由调用方持有（`App` 只持 `Handle`）。
struct Fixture {
    app: App,
    window: WindowId,
}

fn new_app(handle: &tokio::runtime::Handle) -> Fixture {
    let mut app = App::new(handle.clone());
    let window = app.create_window(WindowOptions {
        title: SmolStr::new_static("bench"),
        size: VIEWPORT,
        scale: Scale::ONE,
    });
    Fixture { app, window }
}

fn mount_boxes(fixture: &mut Fixture, count: usize) -> Vec<NodeId> {
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    let mut nodes = Vec::with_capacity(count);
    for index in 0..count {
        let x = (index % 8) as f32 * 96.0;
        let y = (index / 8) as f32 * 32.0;
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(x, y, 90.0, 28.0))
            .paint(PaintSpec {
                background: Color::from_rgba8([0x00, 0x91, 0xFF, 0xFF]),
                corner_radius: [4.0; 4],
                ..PaintSpec::default()
            })
            .build(app.window_mut(*window).expect("window").tree_mut(), root)
            .expect("bench box");
        nodes.push(node);
    }
    nodes
}

fn mount_text(fixture: &mut Fixture, count: usize) -> Signal<SmolStr> {
    let text = fixture
        .app
        .signal(SmolStr::new_static("Kytrix benchmark 0123"));
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    for index in 0..count {
        let y = index as f32 * 20.0;
        let source = text;
        let _ = NodeBuilder::text("Kytrix benchmark 0123", TextStyle::new(13.0))
            .layout_override(Rect::new(0.0, y, 400.0, 18.0))
            .bind_text(move |cx| cx.read(&source))
            .build(app.window_mut(*window).expect("window").tree_mut(), root)
            .expect("bench text");
    }
    text
}

/// 一个被 64 个 `bind_style` 读的信号（依赖失效的扇出）。
fn mount_fanout(fixture: &mut Fixture, count: usize) -> Signal<u8> {
    let signal = fixture.app.signal(0u8);
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    for index in 0..count {
        let y = index as f32 * 12.0;
        let value = signal;
        let _ = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, y, 100.0, 10.0))
            .bind_style(move |cx| StylePatch {
                opacity: Some(f32::from(cx.read(&value)) / 255.0),
                ..StylePatch::default()
            })
            .build(app.window_mut(*window).expect("window").tree_mut(), root)
            .expect("fanout box");
    }
    signal
}

fn palette() -> Vec<Color> {
    let mut colors = Vec::with_capacity(256);
    for index in 0..256u32 {
        let hue = (index as f32 * 1.4) % 360.0;
        colors.push(match index % 4 {
            0 => Color::Srgb {
                r: (index as f32 % 17.0) / 17.0,
                g: 0.42,
                b: 0.18,
                alpha: 1.0,
            },
            1 => Color::Oklch {
                l: 0.55,
                c: 0.22,
                h: hue,
                alpha: 0.8,
            },
            2 => Color::Oklab {
                l: 0.62,
                a: 0.11,
                b: -0.09,
                alpha: 1.0,
            },
            _ => Color::DisplayP3 {
                r: 0.9,
                g: 0.3,
                b: 0.2,
                alpha: 0.5,
            },
        });
    }
    colors
}

fn bench_color_resolve(c: &mut Criterion) {
    let resolver = ColorResolver::new(Interpolation::default());
    let colors = palette();
    c.bench_function("color_resolve_256", |b| {
        b.iter(|| {
            let mut last = resolver.resolve(Color::TRANSPARENT);
            for color in &colors {
                last = resolver.resolve(black_box(*color));
            }
            black_box(last)
        });
    });
}

fn bench_tree_build(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    c.bench_function("tree_build_512", |b| {
        // setup 只建空 `App` + 窗口（**不计时**）；routine 才是 512 次建树。
        b.iter_batched(
            || new_app(&handle),
            |mut fixture| {
                let nodes = mount_boxes(&mut fixture, 512);
                black_box(nodes.len())
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_text_shape(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let mut toggle = 0usize;
    c.bench_function("text_shape_32", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let signal = mount_text(&mut fixture, 32);
                (fixture, signal)
            },
            |(mut fixture, signal)| {
                toggle += 1;
                let text = if toggle.is_multiple_of(2) {
                    SmolStr::new_static("Kytrix benchmark 0123")
                } else {
                    SmolStr::new_static("Kytrix benchmark 3210")
                };
                fixture.app.write(&signal, text);
                let report = fixture.app.refresh_text(fixture.window);
                black_box(report)
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_text_layout(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    c.bench_function("text_layout_32", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let _ = mount_text(&mut fixture, 32);
                let _ = fixture.app.refresh_text(fixture.window);
                fixture
            },
            |mut fixture| black_box(fixture.app.layout_text(fixture.window)),
            BatchSize::SmallInput,
        );
    });
}

fn bench_layout(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    c.bench_function("layout_512", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let _ = mount_boxes(&mut fixture, 512);
                fixture
            },
            |mut fixture| {
                let report = fixture
                    .app
                    .window_mut(fixture.window)
                    .expect("window")
                    .compute_layout();
                black_box(report)
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_frame_full(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let sizes = [VIEWPORT, Size::new(801.0, 601.0)];
    let mut toggle = 0usize;
    c.bench_function("frame_full_512", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let _ = mount_boxes(&mut fixture, 512);
                let _ = fixture.app.frame(fixture.window);
                fixture
            },
            |mut fixture| {
                // 交替尺寸强制全帧 damage（整帧重建 + 编码 + 合并）。
                toggle += 1;
                if let Some(window) = fixture.app.window_mut(fixture.window) {
                    window.set_size(sizes[toggle % 2]);
                }
                black_box(fixture.app.frame(fixture.window))
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_frame_incremental(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let mut toggle = 0usize;
    c.bench_function("frame_incremental_512", |b| {
        b.iter_batched(
            || {
                // 512 个静态 box（lane 可整桶复用）+ 1 个绑了信号的节点（本帧唯一被标脏的）。
                let mut fixture = new_app(&handle);
                let _ = mount_boxes(&mut fixture, 512);
                let signal = mount_fanout(&mut fixture, 1);
                let _ = fixture.app.frame(fixture.window);
                (fixture, signal)
            },
            |(mut fixture, signal)| {
                // 改一个节点：测 lane 复用 + damage 合并的增量帧。
                toggle += 1;
                fixture.app.write(&signal, (toggle % 2) as u8);
                black_box(fixture.app.frame(fixture.window))
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_signal_write(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let mut toggle = 0usize;
    c.bench_function("signal_write_64", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let signal = mount_fanout(&mut fixture, 64);
                (fixture, signal)
            },
            |(mut fixture, signal)| {
                toggle += 1;
                let changed = fixture.app.write(&signal, (toggle % 255) as u8);
                black_box(changed)
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(50)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets =
        bench_color_resolve,
        bench_tree_build,
        bench_text_shape,
        bench_text_layout,
        bench_layout,
        bench_frame_full,
        bench_frame_incremental,
        bench_signal_write
}
criterion_main!(benches);

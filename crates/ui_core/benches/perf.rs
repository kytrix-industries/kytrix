use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use smol_str::{SmolStr, format_smolstr};
#[cfg(windows)]
use ui_core::math::DevicePixelRatio;
use ui_core::{
    a11y::A11yPatch,
    app::App,
    math::{LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        color::{Color, ColorResolver},
        style::{PaintSpec, StylePatch},
    },
    text::TextStyle,
    window::{WindowId, WindowOptions},
};

const VIEWPORT: LogicalSize = LogicalSize::new(px(800.0), px(600.0));

const TEXT_GRID_ROWS: usize = 40;
const TEXT_GRID_COLUMNS: usize = 8;

struct Fixture {
    app: App,
    window: WindowId,
}

fn new_app(handle: &tokio::runtime::Handle) -> Fixture {
    let mut app = App::new(handle.clone());
    let window = app.create_window(WindowOptions {
        title: SmolStr::new_static("bench"),
        size: VIEWPORT,
        ..WindowOptions::default()
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
            .layout_override(LogicalRect::new(px(x), px(y), px(90.0), px(28.0)))
            .paint(PaintSpec {
                background: Color::from_rgba8([0x00, 0x91, 0xFF, 0xFF]),
                corner_radius: [px(4.0); 4],
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
        let _ = NodeBuilder::text("Kytrix benchmark 0123", TextStyle::new(px(13.0)))
            .layout_override(LogicalRect::new(px(0.0), px(y), px(400.0), px(18.0)))
            .bind_text(move |cx| (*cx.read(&source)).clone())
            .build(app.window_mut(*window).expect("window").tree_mut(), root)
            .expect("bench text");
    }
    text
}

fn mount_text_grid(fixture: &mut Fixture) -> Signal<SmolStr> {
    let text = fixture.app.signal(SmolStr::new_static("frame a"));
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    for row in 0..TEXT_GRID_ROWS {
        for column in 0..TEXT_GRID_COLUMNS {
            let source = text;
            let cell = format_smolstr!("{row},{column}");
            let initial = format_smolstr!("{cell} frame a");
            let _ = NodeBuilder::text(initial, TextStyle::new(px(11.0)))
                .layout_override(LogicalRect::new(
                    px(column as f32 * 96.0),
                    px(row as f32 * 14.0),
                    px(92.0),
                    px(13.0),
                ))
                .bind_text(move |cx| format_smolstr!("{cell} {}", *cx.read(&source)))
                .build(app.window_mut(*window).expect("window").tree_mut(), root)
                .expect("bench text cell");
        }
    }
    text
}

fn mount_fanout(fixture: &mut Fixture, count: usize) -> Signal<u8> {
    let signal = fixture.app.signal(0u8);
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    for index in 0..count {
        let y = index as f32 * 12.0;
        let value = signal;
        let _ = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(y), px(100.0), px(10.0)))
            .paint(PaintSpec {
                background: Color::from_rgba8([0xFF, 0xFF, 0xFF, 0xFF]),
                ..PaintSpec::default()
            })
            .bind_style(move |cx| StylePatch {
                opacity: Some(f32::from(*cx.read(&value)) / 255.0),
                ..StylePatch::default()
            })
            .build(app.window_mut(*window).expect("window").tree_mut(), root)
            .expect("fanout box");
    }
    signal
}

#[cfg(feature = "test-support")]
fn assert_incremental_frame(fixture: &Fixture, lanes: usize, label: &str) {
    let Some(stats) = fixture
        .app
        .window(fixture.window)
        .and_then(ui_core::window::Window::frame_stats)
        .copied()
    else {
        return;
    };
    assert_eq!(
        stats.encoded_lanes, 1,
        "{label}: 增量帧只允许重编码 1 条 lane（实际 {}）—— 增量编码退化成全量？",
        stats.encoded_lanes
    );
    assert_eq!(
        stats.reused_segments, lanes,
        "{label}: 其余 {lanes} 条 lane 必须整段复用（实际 {}）",
        stats.reused_segments
    );
    assert_eq!(
        stats.uploaded_bytes, 128,
        "{label}: 增量帧只允许上传 1 个实例 = 128 B（实际 {}）",
        stats.uploaded_bytes
    );
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
    let resolver = ColorResolver;
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
    let sizes = [VIEWPORT, LogicalSize::new(px(801.0), px(601.0))];
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
                toggle += 1;
                if let Some(window) = fixture.app.window_mut(fixture.window) {
                    window.set_size(sizes[toggle % 2]);
                }
                let outcome = black_box(fixture.app.frame(fixture.window));
                (fixture, outcome)
            },
            BatchSize::LargeInput,
        );
    });
}

fn bench_frame_incremental(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let toggle = std::cell::Cell::new(1u8);
    c.bench_function("frame_incremental_512", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let _ = mount_boxes(&mut fixture, 512);
                let signal = mount_fanout(&mut fixture, 1);
                let _ = fixture.app.frame(fixture.window);
                fixture.app.write(&signal, toggle.get());
                (fixture, signal)
            },
            |(mut fixture, signal)| {
                toggle.set(1 + toggle.get() % 254);
                fixture.app.write(&signal, toggle.get());
                let outcome = black_box(fixture.app.frame(fixture.window));
                #[cfg(feature = "test-support")]
                assert_incremental_frame(&fixture, 512, "frame_incremental_512");
                (fixture, outcome)
            },
            BatchSize::LargeInput,
        );
    });
}

fn bench_a11y_only_update(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    assert_a11y_only_is_much_cheaper_than_a_paint_frame(&handle);
    #[cfg(feature = "test-support")]
    let printed = std::sync::Once::new();
    c.bench_function("a11y_only_update_10k", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let _ = mount_boxes(&mut fixture, 10_000);
                let label = mount_a11y_label(&mut fixture);
                let _ = fixture.app.frame(fixture.window);
                fixture.app.write(&label, SmolStr::new_static("a11y label 1"));
                (fixture, label)
            },
            |(mut fixture, label)| {
                fixture.app.write(&label, SmolStr::new_static("a11y label 2"));
                let outcome = black_box(fixture.app.frame(fixture.window));
                #[cfg(feature = "test-support")]
                if let Some(stats) = fixture
                    .app
                    .window(fixture.window)
                    .and_then(ui_core::window::Window::frame_stats)
                    .copied()
                {
                    assert_eq!(
                        stats.encoded_lanes, 0,
                        "a11y-only 帧不得重编码任何 lane（实际 {}）",
                        stats.encoded_lanes
                    );
                    assert_eq!(
                        stats.painted_lanes, 0,
                        "a11y-only 帧不得重建任何 lane（实际 {}）",
                        stats.painted_lanes
                    );
                    assert!(
                        stats.a11y_only_frames >= 1,
                        "a11y_only_frames 必须增长 —— a11y-only 快速路径没跑？"
                    );
                    printed.call_once(|| {
                        eprintln!(
                            "a11y_only_update_10k: encoded_lanes={} painted_lanes={} a11y_only_frames={}",
                            stats.encoded_lanes, stats.painted_lanes, stats.a11y_only_frames
                        );
                    });
                }
                (fixture, outcome)
            },
            BatchSize::LargeInput,
        );
    });
}

fn mount_a11y_label(fixture: &mut Fixture) -> Signal<SmolStr> {
    let label = fixture.app.signal(SmolStr::new_static("a11y label 0"));
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    let source = label;
    let _ = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(160.0), px(24.0)))
        .paint(PaintSpec {
            background: Color::from_rgba8([0x00, 0x91, 0xFF, 0xFF]),
            ..PaintSpec::default()
        })
        .bind_a11y(move |cx| A11yPatch {
            label: Some(cx.read(&source).clone()),
            ..A11yPatch::default()
        })
        .build(app.window_mut(*window).expect("window").tree_mut(), root)
        .expect("a11y label");
    label
}

fn assert_a11y_only_is_much_cheaper_than_a_paint_frame(handle: &tokio::runtime::Handle) {
    let mut fixture = new_app(handle);
    let _ = mount_boxes(&mut fixture, 10_000);
    let label = mount_a11y_label(&mut fixture);
    let painted = mount_fanout(&mut fixture, 1);
    let _ = fixture.app.frame(fixture.window);
    fixture
        .app
        .write(&label, SmolStr::new_static("a11y label 1"));
    fixture.app.write(&painted, 1u8);
    let mut a11y_min = Duration::MAX;
    let mut paint_min = Duration::MAX;
    for round in 0..7usize {
        let value = if round.is_multiple_of(2) {
            SmolStr::new_static("a11y label 2")
        } else {
            SmolStr::new_static("a11y label 1")
        };
        let start = Instant::now();
        fixture.app.write(&label, value);
        let outcome = fixture.app.frame(fixture.window);
        a11y_min = a11y_min.min(start.elapsed());
        black_box(outcome);

        let start = Instant::now();
        fixture.app.write(&painted, 2 + round as u8);
        let outcome = fixture.app.frame(fixture.window);
        paint_min = paint_min.min(start.elapsed());
        black_box(outcome);
    }
    let ratio = paint_min.as_secs_f64() / a11y_min.as_secs_f64();
    eprintln!(
        "a11y_only_update: a11y-only {:.2} µs vs paint {:.2} µs ({ratio:.1}×)",
        a11y_min.as_secs_f64() * 1e6,
        paint_min.as_secs_f64() * 1e6
    );
    assert!(
        a11y_min * 3 < paint_min,
        "a11y-only 帧（{:.2} µs）必须远低于同规模绘制帧（{:.2} µs，{ratio:.1}×）：\
         a11y-only 写又跑回像素管线了？",
        a11y_min.as_secs_f64() * 1e6,
        paint_min.as_secs_f64() * 1e6
    );
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

fn bench_tree_build_scale(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    for (name, count) in [
        ("tree_build_1k", 1_000usize),
        ("tree_build_10k", 10_000usize),
    ] {
        c.bench_function(name, |b| {
            b.iter_batched(
                || new_app(&handle),
                |mut fixture| {
                    let nodes = mount_boxes(&mut fixture, count);
                    black_box(nodes.len())
                },
                BatchSize::SmallInput,
            );
        });
    }
}

fn bench_frame_incremental_scale(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    for (name, count) in [
        ("frame_incremental_1k", 1_000usize),
        ("frame_incremental_10k", 10_000usize),
    ] {
        let toggle = std::cell::Cell::new(1u8);
        #[cfg(feature = "test-support")]
        let stats_printed = std::sync::Once::new();
        c.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let mut fixture = new_app(&handle);
                    let _ = mount_boxes(&mut fixture, count);
                    let signal = mount_fanout(&mut fixture, 1);
                    let _ = fixture.app.frame(fixture.window);
                    fixture.app.write(&signal, toggle.get());
                    (fixture, signal)
                },
                |(mut fixture, signal)| {
                    toggle.set(1 + toggle.get() % 254);
                    fixture.app.write(&signal, toggle.get());
                    let outcome = black_box(fixture.app.frame(fixture.window));
                    #[cfg(feature = "test-support")]
                    {
                        assert_incremental_frame(&fixture, count, name);
                        if let Some(stats) = fixture
                            .app
                            .window(fixture.window)
                            .and_then(ui_core::window::Window::frame_stats)
                            .copied()
                        {
                            stats_printed.call_once(|| {
                                eprintln!(
                                    "{name}: encoded_lanes={} reused_segments={} uploaded_bytes={} instances={}",
                                    stats.encoded_lanes,
                                    stats.reused_segments,
                                    stats.uploaded_bytes,
                                    stats.instances
                                );
                            });
                        }
                    }
                    (fixture, outcome)
                },
                BatchSize::LargeInput,
            );
        });
    }
}

fn bench_frame_full_10k(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let sizes = [VIEWPORT, LogicalSize::new(px(801.0), px(601.0))];
    let mut toggle = 0usize;
    c.bench_function("frame_full_10k", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let _ = mount_boxes(&mut fixture, 10_000);
                let _ = fixture.app.frame(fixture.window);
                fixture
            },
            |mut fixture| {
                toggle += 1;
                if let Some(window) = fixture.app.window_mut(fixture.window) {
                    window.set_size(sizes[toggle % 2]);
                }
                let outcome = black_box(fixture.app.frame(fixture.window));
                (fixture, outcome)
            },
            BatchSize::LargeInput,
        );
    });
}

fn bench_scroll_1px(c: &mut Criterion) {
    use ui_core::{
        interaction::{ScrollConfig, ScrollHandle},
        paint::{style::Clip, style_builder::StyleBuilder},
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    const ROWS: usize = 10_000;
    const ROW_HEIGHT: f32 = 20.0;
    let mut toggle = 0usize;
    c.bench_function("scroll_1px_10k", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let Fixture { app, window } = &mut fixture;
                let root = app.window(*window).expect("window").tree().root();
                let mut style = StyleBuilder::new()
                    .size(VIEWPORT.width, VIEWPORT.height)
                    .build();
                style.clip = Clip::Bounds;
                let viewport = NodeBuilder::boxed()
                    .style(style)
                    .build(app.window_mut(*window).expect("window").tree_mut(), root)
                    .expect("scroll viewport");
                for index in 0..ROWS {
                    NodeBuilder::boxed()
                        .layout_override(LogicalRect::new(
                            px(0.0),
                            px(index as f32 * ROW_HEIGHT),
                            VIEWPORT.width,
                            px(ROW_HEIGHT - 2.0),
                        ))
                        .paint(PaintSpec {
                            background: Color::from_rgba8([0x28, 0x2C, 0x34, 0xFF]),
                            ..PaintSpec::default()
                        })
                        .build(
                            app.window_mut(*window).expect("window").tree_mut(),
                            viewport,
                        )
                        .expect("scroll row");
                }
                let scroll: ScrollHandle = app
                    .make_scroll(*window, viewport, ScrollConfig::default())
                    .expect("scroll container");
                app.set_scroll_content(
                    &scroll,
                    LogicalSize::new(VIEWPORT.width, px(ROWS as f32 * ROW_HEIGHT)),
                )
                .expect("scroll content");
                let _ = app.frame(*window);
                (fixture, scroll)
            },
            |(mut fixture, scroll)| {
                toggle += 1;
                let y = if toggle.is_multiple_of(2) { 1.0 } else { 2.0 };
                fixture
                    .app
                    .scroll_to(&scroll, ui_core::math::LogicalPoint::new(px(0.0), px(y)))
                    .expect("scroll 1px");
                let outcome = black_box(fixture.app.frame(fixture.window));

                #[cfg(feature = "test-support")]
                assert_scroll_frame(&fixture, ROWS, "scroll_1px_10k");
                (fixture, outcome)
            },
            BatchSize::SmallInput,
        );
    });
}

#[cfg(feature = "test-support")]
fn assert_scroll_frame(fixture: &Fixture, lanes: usize, label: &str) {
    let Some(stats) = fixture
        .app
        .window(fixture.window)
        .and_then(ui_core::window::Window::frame_stats)
        .copied()
    else {
        return;
    };

    assert_eq!(
        stats.encoded_lanes, 0,
        "{label}: 滚动帧不得重编码任何 lane（实际 {}）—— 槽位接线断了？",
        stats.encoded_lanes
    );
    assert_eq!(
        stats.reused_segments, lanes,
        "{label}: 全部 {lanes} 条 lane 必须整段复用（实际 {}）",
        stats.reused_segments
    );
    assert_eq!(
        stats.uploaded_bytes, 0,
        "{label}: 滚动帧不得上传任何实例字节（实际 {}）",
        stats.uploaded_bytes
    );
    assert_eq!(
        stats.scroll_offset_frames, 1,
        "{label}: 偏移变化必须进 delta（实际 {} 个槽）—— 滚动会停在旧偏移上",
        stats.scroll_offset_frames
    );
}

fn bench_text_update_320(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let values = [
        SmolStr::new_static("frame a"),
        SmolStr::new_static("frame b"),
    ];
    let mut toggle = 0usize;
    c.bench_function("text_update_320", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let signal = mount_text_grid(&mut fixture);
                let _ = fixture.app.frame(fixture.window);
                (fixture, signal)
            },
            |(mut fixture, signal)| {
                toggle += 1;
                fixture.app.write(&signal, values[toggle % 2].clone());
                let outcome = black_box(fixture.app.frame(fixture.window));
                (fixture, outcome)
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_text_pass_320(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    for (name, values) in [
        (
            "text_pass_320_parley",
            [
                SmolStr::new_static("frame a"),
                SmolStr::new_static("frame b"),
            ],
        ),
        (
            "text_pass_320_digit",
            [
                SmolStr::new_static("1,234.56"),
                SmolStr::new_static("1,234.57"),
            ],
        ),
    ] {
        let mut toggle = 0usize;
        c.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let mut fixture = new_app(&handle);
                    let signal = if name.ends_with("digit") {
                        mount_digit_grid(&mut fixture)
                    } else {
                        mount_text_grid(&mut fixture)
                    };
                    let _ = fixture.app.frame(fixture.window);
                    (fixture, signal)
                },
                |(mut fixture, signal)| {
                    toggle += 1;
                    fixture.app.write(&signal, values[toggle % 2].clone());
                    black_box(fixture.app.refresh_text(fixture.window))
                },
                BatchSize::SmallInput,
            );
        });
    }
}

fn mount_digit_grid(fixture: &mut Fixture) -> Signal<SmolStr> {
    let text = fixture.app.signal(SmolStr::new_static("1,234.56"));
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    for row in 0..TEXT_GRID_ROWS {
        for column in 0..TEXT_GRID_COLUMNS {
            let source = text;
            let _ = NodeBuilder::text(
                "1,234.56",
                TextStyle::new(px(11.0)).with_wrap(ui_core::text::TextWrap::NoWrap),
            )
            .layout_override(LogicalRect::new(
                px(column as f32 * 96.0),
                px(row as f32 * 14.0),
                px(92.0),
                px(13.0),
            ))
            .bind_text(move |cx| (*cx.read(&source)).clone())
            .build(app.window_mut(*window).expect("window").tree_mut(), root)
            .expect("bench digit cell");
        }
    }
    text
}

fn bench_digit_fast_path_320(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let values = [
        SmolStr::new_static("1,234.56"),
        SmolStr::new_static("1,234.57"),
    ];
    for (name, enabled) in [
        ("digit_fast_path_320", true),
        ("digit_fast_path_off_320", false),
    ] {
        let mut toggle = 0usize;
        c.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let mut fixture = new_app(&handle);
                    if !enabled {
                        fixture.app.set_digit_fast_path(false);
                    }
                    let signal = mount_digit_grid(&mut fixture);
                    let _ = fixture.app.frame(fixture.window);
                    (fixture, signal)
                },
                |(mut fixture, signal)| {
                    toggle += 1;
                    fixture.app.write(&signal, values[toggle % 2].clone());
                    let outcome = black_box(fixture.app.frame(fixture.window));
                    (fixture, outcome)
                },
                BatchSize::SmallInput,
            );
        });
    }
}

fn bench_frame_incremental_10k_ui_thread(c: &mut Criterion) {
    #[cfg(not(feature = "test-support"))]
    {
        let _ = c;
    }
    #[cfg(feature = "test-support")]
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("bench runtime");
        let handle = runtime.handle().clone();
        let count = 10_000usize;
        let toggle = std::cell::Cell::new(1u8);
        c.bench_function("frame_incremental_10k_ui_thread", |b| {
            b.iter_batched(
                || {
                    let mut fixture = new_app(&handle);
                    let _ = mount_boxes(&mut fixture, count);
                    let signal = mount_fanout(&mut fixture, 1);
                    let _ = fixture.app.frame(fixture.window);
                    fixture.app.write(&signal, toggle.get());
                    if let Some(window) = fixture.app.window_mut(fixture.window) {
                        window.detach_composer();
                    }
                    (fixture, signal)
                },
                |(mut fixture, signal)| {
                    toggle.set(1 + toggle.get() % 254);
                    fixture.app.write(&signal, toggle.get());
                    let outcome = black_box(fixture.app.frame(fixture.window));
                    (fixture, outcome)
                },
                BatchSize::LargeInput,
            );
        });
    }
}

fn bench_idle_frame(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    c.bench_function("idle_frame", |b| {
        b.iter_batched(
            || {
                let mut fixture = new_app(&handle);
                let _ = mount_boxes(&mut fixture, 1_000);
                let _ = fixture.app.frame(fixture.window);
                fixture
            },
            |mut fixture| {
                let outcome = black_box(fixture.app.frame(fixture.window));
                (fixture, outcome)
            },
            BatchSize::LargeInput,
        );
    });
}

#[cfg(feature = "hud")]
fn bench_hud_idle_frame(c: &mut Criterion) {
    use ui_core::window::FrameOutcome;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let mut fixture = new_app(&handle);
    let _ = mount_boxes(&mut fixture, 1);
    fixture
        .app
        .set_debug_hud(fixture.window, true)
        .expect("install debug hud");
    let _ = fixture.app.frame(fixture.window);
    let reported = std::sync::Once::new();
    c.bench_function("hud_idle_frame", |b| {
        b.iter(|| {
            let outcome = black_box(fixture.app.frame(fixture.window));
            reported.call_once(|| {
                let committed = matches!(
                    outcome.as_ref().and_then(|result| result.as_ref().ok()),
                    Some(FrameOutcome::Committed(_))
                );
                eprintln!(
                    "hud_idle_frame: 静止帧 {}",
                    if committed {
                        "提交（HUD 又变成了帧源）"
                    } else {
                        "Idle（HUD 不产生帧）"
                    }
                );
            });
            outcome
        });
    });
}

#[cfg(not(feature = "hud"))]
fn bench_hud_idle_frame(_c: &mut Criterion) {}

#[cfg(windows)]
fn bench_pump_composition_geometry(c: &mut Criterion) {
    use windows::{
        Win32::{
            Foundation::{HWND, RECT},
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, GetClientRect, IsIconic, WINDOW_EX_STYLE, WS_POPUP,
            },
        },
        core::w,
    };

    let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
        return;
    };
    let hwnd: HWND = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("kytrix bench target"),
            WS_POPUP,
            0,
            0,
            800,
            600,
            None,
            None,
            Some(instance.into()),
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(_) => return,
    };
    let mut group = c.benchmark_group("pump_composition_geometry");
    for windows_count in [1usize, 64] {
        group.bench_function(format!("syscalls_{windows_count}_windows"), |b| {
            b.iter(|| {
                let mut last = (false, 0i32, 0i32);
                for _ in 0..windows_count {
                    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
                    let mut rect = RECT::default();
                    let _ = unsafe { GetClientRect(hwnd, &mut rect) };
                    last = (minimized, rect.right, rect.bottom);
                }
                black_box(last)
            });
        });
        group.bench_function(format!("cached_{windows_count}_windows"), |b| {
            let key = (VIEWPORT, DevicePixelRatio::ONE);
            let cached = key;
            b.iter(|| {
                let mut hits = 0usize;
                for _ in 0..windows_count {
                    let probe = black_box(key);
                    if probe == cached {
                        hits += 1;
                    }
                }
                black_box(hits)
            });
        });
    }
    group.finish();
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

#[cfg(not(windows))]
fn bench_pump_composition_geometry(_c: &mut Criterion) {}

criterion_group! {
    name = rw11_benches;
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    targets = bench_hud_idle_frame, bench_pump_composition_geometry
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
        bench_a11y_only_update,
        bench_signal_write,
        bench_tree_build_scale,
        bench_frame_incremental_scale,
        bench_frame_full_10k,
        bench_scroll_1px,
        bench_text_update_320,
        bench_text_pass_320,
        bench_digit_fast_path_320,
        bench_frame_incremental_10k_ui_thread,
        bench_idle_frame
}
criterion_main!(benches, rw11_benches);

//! L4 性能基准（criterion；**非合并门禁**，只要求 `cargo bench --no-run` 可编译）。
//!
//! 覆盖主设计 §6.7 的六项：颜色转换 / 塑形 / 布局 / 实例构建 / damage 合并 / 依赖失效。
//! 后两项没有独立的公开入口（`DamageAccumulator` 与 `FrameEncoder` 是 crate 私有，
//! 不应为 bench 扩公开面），因此由 `frame_*` 组顺带测量——见
//! `design/phase-13-resources-snapshot.md` §10.2。
//!
//! 规模档位（大整改的基线面）：`tree_build_{1k,10k}`、`frame_incremental_{1k,10k}`、
//! `frame_full_10k`、`text_update_320`（40 行 × 8 列，每帧全部变化）。
//! 基准只读引擎的公开 API，**不**为好看改生产代码；数值不进 CI 判定（D19）。
//!
//! 可复现性：headless（`renderer = None`）⇒ 不受窗口前台化 / 组合器影响；输入全字面量；
//! 每组在 setup 里建 `current_thread` runtime + `App`（**不计时**，`iter_batched` 的 setup 段）。
//! 采样：50 samples / 1 s warmup / 3 s 测量——`frame_*` 单次 ~1 ms、setup ~2 ms，50 是
//! 「统计稳定」与「总时长 < 5 min」的折中；数值**不进 CI 判定**（D19）。
//!
//! ## 增量帧基准的两条硬约束（别再把它们测成噪声）
//!
//! 1. **计时帧必须是真实增量帧**：setup 预置「计时帧写入值的前一个值」，routine 再写新值。
//!    否则 `toggle % 2` 在「每样本新建 fixture」下有一半样本写入与初始值相同的值 ⇒ 帧门禁
//!    直接返回 `Idle`，criterion 拿到的是 2 µs 的空帧与 400 µs 的真帧混合的分布（CI 宽达
//!    一个数量级，197 µs / 553 µs 这类数字就是这么来的）。
//! 2. **脏节点必须有背景**：没有图元的节点被标脏后重建出的是空 lane，编码器不会重编码任何
//!    实例 —— 计时帧的 `encoded_lanes` 实际是 0（旧夹具的 `encoded_lanes=1` 来自另一个统计
//!    探针帧，不是被计时的那一帧）。
//!
//! ## `frame_incremental_10k` 为什么只能跑小 sample size
//!
//! 10k 夹具的 setup（建树 + 首帧布局/样式/全量编码）约 440 ms/样本，而 criterion 的 Linear
//! 采样按 `d = measurement_time / mean / (n(n+1)/2)` 取整：`n=100` 时 d=1 ⇒ 要跑 5050 次
//! setup（约 37 min）。因此 10k 用 `--sample-size 10~30`，统计稳定性靠**多跑几轮**而不是
//! 单轮样本数；1k 与 512 可以用 `--sample-size 100 --measurement-time 10`。
//! 另外 `--features test-support` 会在**每一帧**做 `FrameRecord` 采集（遍历 scene/tree），
//! 同一档位的数值会比默认特性高一个数量级——**两者不可直接比较**，只用于断言计数。

use std::{hint::black_box, time::Duration};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    math::{Rect, Scale, Size},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        color::{Color, ColorResolver},
        style::{PaintSpec, StylePatch},
    },
    text::TextStyle,
    window::{WindowId, WindowOptions},
};

/// 逻辑视口（固定，避免窗口尺寸影响布局成本）。
const VIEWPORT: Size = Size::new(800.0, 600.0);

/// 文本网格：40 行 × 8 列 = 320 个文本节点（`text_update_320`）。
const TEXT_GRID_ROWS: usize = 40;
const TEXT_GRID_COLUMNS: usize = 8;

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
            .bind_text(move |cx| (*cx.read(&source)).clone())
            .build(app.window_mut(*window).expect("window").tree_mut(), root)
            .expect("bench text");
    }
    text
}

/// 40 行 × 8 列的文本网格：每个单元格的文本都读同一个信号，写一次信号 ⇒ 320 个节点全部重排重塑形。
fn mount_text_grid(fixture: &mut Fixture) -> Signal<SmolStr> {
    let text = fixture.app.signal(SmolStr::new_static("frame a"));
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    for row in 0..TEXT_GRID_ROWS {
        for column in 0..TEXT_GRID_COLUMNS {
            let source = text;
            let cell = format_smolstr!("{row},{column}");
            let initial = format_smolstr!("{cell} frame a");
            let _ = NodeBuilder::text(initial, TextStyle::new(11.0))
                .layout_override(Rect::new(
                    column as f32 * 96.0,
                    row as f32 * 14.0,
                    92.0,
                    13.0,
                ))
                .bind_text(move |cx| format_smolstr!("{cell} {}", *cx.read(&source)))
                .build(app.window_mut(*window).expect("window").tree_mut(), root)
                .expect("bench text cell");
        }
    }
    text
}

/// 一个被 `count` 个 `bind_style` 读的信号（依赖失效的扇出 + 增量帧的唯一脏节点）。
///
/// **必须有背景**：只有「改样式会改变图元」的节点才会让 lane 重建 + 实例重编码。没有背景的
/// 盒子即使被标脏，重建出的 lane 也是空的 —— 编码器根本不会重编码任何实例，基准就测不到
/// 「增量编码」这件事（旧夹具正是如此：`frame_incremental_*` 的计时帧里 `encoded_lanes`
/// 实际是 0，`encoded_lanes=1` 只出现在额外的统计探针上）。
fn mount_fanout(fixture: &mut Fixture, count: usize) -> Signal<u8> {
    let signal = fixture.app.signal(0u8);
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    for index in 0..count {
        let y = index as f32 * 12.0;
        let value = signal;
        let _ = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, y, 100.0, 10.0))
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

/// 基准侧的增量编码回归防线（**超标直接 panic**）。
///
/// 判据来自 `Window::frame_stats`（`test-support` 的记录面读点）。三个事实缺一不可：
/// 1. `encoded_lanes == 1`：只有脏的那一条 lane 被重编码（不是全量）；
/// 2. `reused_segments == lanes`：其余每条 lane 的实例段**整段复用**（一个实例字节都没重写）；
/// 3. `uploaded_bytes == 128`：载荷只带那一条 lane 的 1 个实例（128 B 实例布局）。
///
/// 这比「时间变慢」更早、更确定地暴露回归：时间受机器负载影响 ±40%，但计数是确定的。
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
                let outcome = black_box(fixture.app.frame(fixture.window));
                // **必须把 fixture 作为输出交回**：routine 拥有它时 drop 落在计时区内，
                // 10k 节点树的析构（~0.9 ms）会把帧成本淹没。交回后 criterion 在计时区外
                // 释放输出（`LargeInput` 只多留 1 个 fixture 在内存里）。
                (fixture, outcome)
            },
            // 帧的成本与输入规模无关地大，但 batch 里只保留 1 个 fixture：`LargeInput` 的
            // 批量是按 `iters/1000` 取整（这里恒 1），既不计 setup 也不计 teardown。
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
    // 递增而不是取模：(k+1)%2 与 setup 预置的值相同，会让**每两个样本**测到一次 Idle 帧。
    // 1..=254：**始终可见**（opacity=0 会让那条 lane 变成空 lane，基准会跑到「无实例」的边上）。
    let toggle = std::cell::Cell::new(1u8);
    c.bench_function("frame_incremental_512", |b| {
        b.iter_batched(
            || {
                // 512 个静态 box（lane 可整桶复用）+ 1 个绑了信号的节点（本帧唯一被标脏的）。
                let mut fixture = new_app(&handle);
                let _ = mount_boxes(&mut fixture, 512);
                let signal = mount_fanout(&mut fixture, 1);
                let _ = fixture.app.frame(fixture.window);
                // setup 预置「本帧将要写入的值之前的那一个值」⇒ 计时帧 100% 是真实增量帧。
                fixture.app.write(&signal, toggle.get());
                (fixture, signal)
            },
            |(mut fixture, signal)| {
                // 改一个节点：测 lane 复用 + damage 合并的增量帧。
                toggle.set(1 + toggle.get() % 254);
                fixture.app.write(&signal, toggle.get());
                let outcome = black_box(fixture.app.frame(fixture.window));
                #[cfg(feature = "test-support")]
                assert_incremental_frame(&fixture, 512, "frame_incremental_512");
                // 见 `frame_full_512`：fixture 必须交回，否则 drop 落在计时区内。
                (fixture, outcome)
            },
            // 测帧本身（teardown / setup 不计时，见 `frame_full_512`）。
            BatchSize::LargeInput,
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

/// 建树规模：1k / 10k 个绝对定位 box（不含布局成本，纯节点 + 树结构）。
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

/// 增量帧规模：`count` 个静态 box（lane 可整桶复用）+ 1 个绑信号的节点（本帧唯一被标脏的）。
///
/// 计时帧一定是**真实增量帧**：setup 把信号预置成「计时帧写入值的前一个值」，于是计时帧必然
/// 重建 1 条 lane。旧夹具的 `toggle % 2` 在「每样本新建 fixture」下会让**一半样本落到 Idle
/// 帧**（写入值与初始值相同 ⇒ 无脏位 ⇒ 帧门禁直接返回），criterion 的均值因此毫无意义
/// （CI 宽达一个数量级；`frame_incremental_10k` 的 197 µs / 553 µs 都是这样量出来的）。
///
/// `--features test-support` 下 routine 里还会断言 `encoded_lanes=1 / reused_segments=count /
/// uploaded_bytes=128`（见 [`assert_incremental_frame`]）：增量编码一旦退化成全量，基准直接 panic。
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
        // 1..=254：**始终可见**（opacity=0 会让那条 lane 变成空 lane，基准会跑到「无实例」的边上）。
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
                    // 预置「前一个值」⇒ 计时帧一定改变样式、一定重建那条 lane。
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
                    // 见 `frame_full_512`：fixture 必须交回，否则 drop 落在计时区内。
                    (fixture, outcome)
                },
                // 测帧本身（teardown / setup 不计时，见 `frame_full_512`）。
                BatchSize::LargeInput,
            );
        });
    }
}

/// 全帧 10k：交替窗口尺寸强制全帧 damage（整帧重建 + 编码 + 合并）。
fn bench_frame_full_10k(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let handle = runtime.handle().clone();
    let sizes = [VIEWPORT, Size::new(801.0, 601.0)];
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
                // 见 `frame_full_512`：fixture 必须交回，否则 drop 落在计时区内。
                (fixture, outcome)
            },
            // 测帧本身（teardown / setup 不计时，见 `frame_full_512`）。
            BatchSize::LargeInput,
        );
    });
}

/// 滚动 1 px：10k **有图元**的行挂在一个滚动容器里，每帧只改 1 px 偏移。
///
/// 这是 `scroll_1px_10k`（架构决策 §7 的「滚动不该重建任何 lane」）的基准。两条历史坑：
/// 1. 行必须**有图元**：空行不产生 lane，`encoded_lanes == 0` 就变得恒真（复审 §7 的
///    「空变化集下编码与否取值相同」）；
/// 2. `fixture` 必须**交回**调用方（`(fixture, outcome)`），否则在一个 10k 节点的 `App`
///    里，它的析构会落进计时区 —— 旧版本因此报出 4.11 ms，而真实帧只有 ~0.1 ms
///    （与 `frame_full_512` 的注释同一个坑）。
fn bench_scroll_1px(c: &mut Criterion) {
    use ui_core::{
        interaction::{ScrollConfig, ScrollHandle},
        paint::style::{Clip, Style},
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
                let mut inner = taffy::Style::<SmolStr>::DEFAULT;
                inner.size = taffy::geometry::Size {
                    width: taffy::style::Dimension::length(VIEWPORT.width),
                    height: taffy::style::Dimension::length(VIEWPORT.height),
                };
                let viewport = NodeBuilder::boxed()
                    .style(Style {
                        inner,
                        clip: Clip::Bounds,
                    })
                    .build(app.window_mut(*window).expect("window").tree_mut(), root)
                    .expect("scroll viewport");
                for index in 0..ROWS {
                    NodeBuilder::boxed()
                        .layout_override(Rect::new(
                            0.0,
                            index as f32 * ROW_HEIGHT,
                            VIEWPORT.width,
                            ROW_HEIGHT - 2.0,
                        ))
                        // 每行一个实色 quad ⇒ 10k 条真实 lane（滚动帧必须一条都不重建）。
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
                    Size::new(VIEWPORT.width, ROWS as f32 * ROW_HEIGHT),
                )
                .expect("scroll content");
                let _ = app.frame(*window);
                (fixture, scroll)
            },
            |(mut fixture, scroll)| {
                toggle += 1;
                // 偏移在 **1 ↔ 2 px** 之间交替：与 setup 留下的 0 也不同 ⇒ 每个计时帧都是
                // 真实滚动帧。旧版本用 0 ↔ 1，一半样本会落到「同值 ⇒ no-op ⇒ Idle 帧」上
                // （与 `frame_incremental_*` 注释里记的坑完全相同）。
                let y = if toggle.is_multiple_of(2) { 1.0 } else { 2.0 };
                fixture
                    .app
                    .scroll_to(&scroll, ui_core::math::Point::new(0.0, y))
                    .expect("scroll 1px");
                let outcome = black_box(fixture.app.frame(fixture.window));

                #[cfg(feature = "test-support")]
                assert_scroll_frame(&fixture, ROWS, "scroll_1px_10k");
                // fixture 必须交回：否则 10k 节点的析构落在计时区内（见本函数文档）。
                (fixture, outcome)
            },
            BatchSize::SmallInput,
        );
    });
}

/// 滚动帧的**计数**断言（仅 test-support）：确定性，不受机器负载影响。
///
/// 复审 AR4 P1-3 的验收口径：滚动 1 px **不得重建 / 重编码任何 lane**，偏移变化必须真的
/// 进 `SceneDelta`（`scroll_offset_frames == 1`）。接线断开时三条断言全部为假。
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

/// 文本全量更新：320 个文本节点每帧全部变化（塑形 + 文本重排 + 绘制）。
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
                // 预热：首帧把 320 个节点全部塑形（本帧不计时）。
                let _ = fixture.app.frame(fixture.window);
                (fixture, signal)
            },
            |(mut fixture, signal)| {
                toggle += 1;
                // `SmolStr` 是内联短串（≤22 字节）⇒ clone 是一次 memcpy，不产生堆分配。
                fixture.app.write(&signal, values[toggle % 2].clone());
                let outcome = black_box(fixture.app.frame(fixture.window));
                // fixture 必须交回：否则 320 节点 + 文本缓存的析构落在计时区内。
                (fixture, outcome)
            },
            BatchSize::SmallInput,
        );
    });
}

/// 只测**文本 pass**（shape 阶段）的收益：320 个单元格全变，但不跑 layout/paint/encode。
///
/// - `text_pass_320_parley`：字母文本（必然走 parley）；
/// - `text_pass_320_digit`：数字文本（`DigitFastPath` 命中）。
///
/// 两者的差值就是「跳过 parley itemize/shaping」的直接收益；`digit_fast_path_*` 那两条是
/// 全帧基准（被布局/绘制/编码摊薄，收益比例自然更小）。
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

/// 挂载 `TEXT_GRID_ROWS × TEXT_GRID_COLUMNS` 的数字网格（`text_update_320` 的夹具）。
fn mount_digit_grid(fixture: &mut Fixture) -> Signal<SmolStr> {
    let text = fixture.app.signal(SmolStr::new_static("1,234.56"));
    let Fixture { app, window } = fixture;
    let root = app.window(*window).expect("window").tree().root();
    for row in 0..TEXT_GRID_ROWS {
        for column in 0..TEXT_GRID_COLUMNS {
            let source = text;
            let _ = NodeBuilder::text(
                "1,234.56",
                TextStyle::new(11.0).with_wrap(ui_core::text::TextWrap::NoWrap),
            )
            .layout_override(Rect::new(
                column as f32 * 96.0,
                row as f32 * 14.0,
                92.0,
                13.0,
            ))
            .bind_text(move |cx| (*cx.read(&source)).clone())
            .build(app.window_mut(*window).expect("window").tree_mut(), root)
            .expect("bench digit cell");
        }
    }
    text
}

/// `DigitFastPath` 的收益基准：同样的 320 个单元格全变，但文本落在数字字符集里。
///
/// - `digit_fast_path_320`：快速路径**开启**（默认）—— 命中时跳过 parley 的 itemize/shaping；
/// - `digit_fast_path_off_320`：同一场景显式关闭快速路径（`App::set_digit_fast_path(false)`），
///   作为同机同场景的对照基线。
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
                    // fixture 必须交回：否则 320 节点的析构落在计时区内。
                    (fixture, outcome)
                },
                BatchSize::SmallInput,
            );
        });
    }
}

/// **B6-2：UI 线程 vs 合成线程的工作拆分**（`--features test-support`）。
///
/// 同一个 fixture 跑两次：
/// - `frame_incremental_10k`（上面那条）：inline 合成 ⇒ UI 线程 = 绘制 + **编码** + 载荷；
/// - `frame_incremental_10k_ui_thread`：摘掉 inline 合成器 ⇒ UI 线程只到 `SceneDelta` 为止
///   （编码与载荷构造发生在合成线程里，本机 headless 没有下游可分发的载荷）。
///
/// 两个数的差值就是「编码搬离 UI 线程」省下的时间；`encoded_lanes` 仍为 0 证明增量复用没退化。
fn bench_frame_incremental_10k_ui_thread(c: &mut Criterion) {
    #[cfg(not(feature = "test-support"))]
    {
        // 摘掉 inline 合成器是 `test-support` 的测试入口；默认特性下这条基准不存在。
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
        // 递增而不是取模：见 `bench_frame_incremental_scale`（否则一半样本是 Idle 帧）。
        // 1..=254：**始终可见**（opacity=0 会让那条 lane 变成空 lane，基准会跑到「无实例」的边上）。
        let toggle = std::cell::Cell::new(1u8);
        c.bench_function("frame_incremental_10k_ui_thread", |b| {
            b.iter_batched(
                || {
                    let mut fixture = new_app(&handle);
                    let _ = mount_boxes(&mut fixture, count);
                    let signal = mount_fanout(&mut fixture, 1);
                    let _ = fixture.app.frame(fixture.window);
                    // setup 预置「前一个值」⇒ 计时帧一定是真实增量帧。
                    fixture.app.write(&signal, toggle.get());
                    // 关键：编码器不在 UI 线程（模拟真实窗口的合成线程路径）。
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
                // 先例 `frame_incremental_10k`：fixture 必须交回，否则 drop 落在计时区内。
                BatchSize::LargeInput,
            );
        });
    }
}

/// 空转：`idle` 场景的 UI 线程开销（B6-2 的「空闲 CPU <1%」在进程层面由合成线程的空闲等待
/// 保证；这条基准守住 UI 侧「没有脏位就不做任何工作」）。
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
                // 没有任何脏位：帧门禁必须直接退化为 Idle（零工作）。
                let outcome = black_box(fixture.app.frame(fixture.window));
                (fixture, outcome)
            },
            BatchSize::LargeInput,
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
criterion_main!(benches);

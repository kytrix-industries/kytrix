use std::{
    alloc::{GlobalAlloc, Layout, System},
    borrow::Cow,
    fmt,
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use criterion::{Criterion, criterion_group, criterion_main};
use pulldown_cmark::{Options, Parser};
use ui_core::{
    node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
    paint::style_builder::StyleBuilder,
    test_support::app::TestApp,
    window::WindowId,
};
use ui_kit::{
    kit::{
        assets,
        markdown::{MarkdownAssets, MarkdownNode, plain_text},
    },
    theme::{appearance::Appearance, handle::ThemeHandle},
};

const RELEASE_EN: &str = include_str!("../../../release_notes/v0.0.3/en.md");
const I18N_FIXTURE: &str = include_str!("../src/kit/markdown/tests/fixtures/i18n_markdown.rs");
const SYNTHETIC_BYTES: usize = 100 * 1024;
const TABLE_ROWS: usize = 10_000;
const TABLE_COLUMNS: usize = 3;
const WIDE_TABLE_ROWS: usize = 200;
const WIDE_TABLE_COLUMNS: usize = 20;
const SECTION: &str = "## Section\n\nParagraph with **bold**, *italic*, `code`, ~~strike~~ and a [link](https://example.com).\n\n- first item\n- second item\n\n> quoted line\n\n```rust\nfn main() {}\n```\n\n| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n\n";
const DECORATION_MAX_HEIGHT: f32 = 2.0;

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record_allocation(size: usize) {
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    ALLOCATED_BYTES.fetch_add(size, Ordering::Relaxed);
    let live = LIVE_BYTES.fetch_add(size, Ordering::Relaxed) + size;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let grown = new_size >= layout.size();
        let delta = new_size.abs_diff(layout.size());
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            if grown {
                ALLOCATED_BYTES.fetch_add(delta, Ordering::Relaxed);
                let live = LIVE_BYTES.fetch_add(delta, Ordering::Relaxed) + delta;
                PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE_BYTES.fetch_sub(delta, Ordering::Relaxed);
            }
        }
        new_pointer
    }
}

fn reset_allocations() -> usize {
    let live = LIVE_BYTES.load(Ordering::Relaxed);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    PEAK_BYTES.store(live, Ordering::Relaxed);
    live
}

fn allocation_report(baseline: usize) -> (usize, usize, usize) {
    (
        ALLOCATIONS.load(Ordering::Relaxed),
        ALLOCATED_BYTES.load(Ordering::Relaxed),
        PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(baseline),
    )
}

struct EmbeddedAssets;

impl MarkdownAssets for EmbeddedAssets {
    fn load(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        assets::bytes(path).map(Cow::Borrowed)
    }
}

struct Fixture {
    app: TestApp,
    theme: ThemeHandle,
    window: WindowId,
    parent: NodeId,
}

fn fixture(production_page: bool) -> Fixture {
    let mut app = TestApp::new();
    let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
    let window = app.window_id();
    let window_root = app.app().window(window).expect("window").tree().root();
    let parent = if production_page {
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .min_width_0()
                    .flex_none()
                    .build(),
            )
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                window_root,
            )
            .expect("page container")
    } else {
        window_root
    };
    Fixture {
        app,
        theme,
        window,
        parent,
    }
}

fn mount(fixture: &mut Fixture, source: &str) -> MarkdownNode {
    MarkdownNode::mount(
        fixture.app.app_mut(),
        fixture.window,
        fixture.parent,
        fixture.theme.clone(),
        source,
        EmbeddedAssets,
    )
    .expect("markdown mounts")
}

struct Metrics {
    nodes: usize,
    spacers: usize,
    spacer_min: f32,
    spacer_max: f32,
    runs: usize,
    decoration_quads: usize,
    background_quads: usize,
    other_quads: usize,
    nested_parses: usize,
    allocations: usize,
    allocated_bytes: usize,
    peak_bytes: usize,
}

impl Metrics {
    fn collect(
        app: &TestApp,
        root: NodeId,
        source: &str,
        allocations: (usize, usize, usize),
    ) -> Self {
        let tree = app.app().window(app.window_id()).expect("window").tree();
        let mut nodes = 0;
        let mut spacers = 0;
        let mut spacer_min = f32::INFINITY;
        let mut spacer_max = 0.0_f32;
        let mut runs = 0;
        let mut text_nodes = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            nodes += 1;
            let entry = tree.get(node).expect("live node");
            if let NodeKind::Text(content) = entry.kind() {
                text_nodes.push(node);
                runs += content.runs().len();
            }
            if tree.debug_id(node) == Some("MARKDOWN_SPACER") {
                spacers += 1;
                let height = entry.layout().bounds.size.height.value();
                spacer_min = spacer_min.min(height);
                spacer_max = spacer_max.max(height);
            }
            stack.extend(entry.children().iter().copied());
        }
        let mut decoration_quads = 0;
        let mut background_quads = 0;
        let mut other_quads = 0;
        if let Some(frame) = app.rendered() {
            for quad in &frame.quads {
                if text_nodes.contains(&quad.node) {
                    if quad.bounds.size.height.value() <= DECORATION_MAX_HEIGHT {
                        decoration_quads += 1;
                    } else {
                        background_quads += 1;
                    }
                } else {
                    other_quads += 1;
                }
            }
        }
        if spacers == 0 {
            spacer_min = 0.0;
        }
        Self {
            nodes,
            spacers,
            spacer_min,
            spacer_max,
            runs,
            decoration_quads,
            background_quads,
            other_quads,
            nested_parses: nested_parse_units(source),
            allocations: allocations.0,
            allocated_bytes: allocations.1,
            peak_bytes: allocations.2,
        }
    }
}

impl fmt::Display for Metrics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "nodes={} spacers={} spacer_height={:.2}..{:.2} runs={} decoration_quads={} background_quads={} other_quads={} nested_parses={} allocations={} alloc_bytes={} peak_bytes={}",
            self.nodes,
            self.spacers,
            self.spacer_min,
            self.spacer_max,
            self.runs,
            self.decoration_quads,
            self.background_quads,
            self.other_quads,
            self.nested_parses,
            self.allocations,
            self.allocated_bytes,
            self.peak_bytes
        )
    }
}

fn nested_parse_units(source: &str) -> usize {
    ["<p ", "<div ", "<center", "<section"]
        .iter()
        .map(|tag| source.matches(tag).count())
        .sum()
}

fn report(fixture: &mut Fixture, label: &str, source: &str) {
    let baseline = reset_allocations();
    let mut node = mount(fixture, source);
    let allocations = allocation_report(baseline);
    let _ = fixture.app.drain().expect("report frame");
    let metrics = Metrics::collect(&fixture.app, node.root(), source, allocations);
    eprintln!("{label}: source_bytes={} {metrics}", source.len());
    node.release(fixture.app.app_mut());
}

fn gfm_options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

fn parse_all(source: &str) -> usize {
    let mut events = 0;
    for event in Parser::new_ext(source, gfm_options()) {
        black_box(&event);
        events += 1;
    }
    events
}

fn synthetic_markdown() -> String {
    let mut source = String::with_capacity(SYNTHETIC_BYTES + SECTION.len());
    while source.len() < SYNTHETIC_BYTES {
        source.push_str(SECTION);
    }
    source
}

fn table_source(rows: usize, columns: usize) -> String {
    let mut source = String::with_capacity(rows * columns * 14);
    source.push('|');
    for column in 0..columns {
        source.push_str(&format!(" h{column} |"));
    }
    source.push('\n');
    source.push('|');
    for _ in 0..columns {
        source.push_str("---|");
    }
    source.push('\n');
    for row in 0..rows {
        source.push('|');
        for column in 0..columns {
            source.push_str(&format!(" r{row}c{column} |"));
        }
        source.push('\n');
    }
    source
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn notice_source() -> String {
    I18N_FIXTURE
        .lines()
        .filter_map(|line| line.trim().strip_prefix("en: \""))
        .filter_map(|value| value.strip_suffix("\","))
        .filter(|value| value.starts_with("<p align="))
        .max_by_key(|value| value.len())
        .map(unescape)
        .expect("the fixture carries one html notice")
}

fn bench_parse_only_7kb(c: &mut Criterion) {
    c.bench_function("parse_only_7kb", |b| {
        b.iter(|| black_box(parse_all(black_box(RELEASE_EN))));
    });
}

fn bench_parse_only_100kb(c: &mut Criterion) {
    let source = synthetic_markdown();
    eprintln!("parse_only_100kb: source_bytes={}", source.len());
    c.bench_function("parse_only_100kb", |b| {
        b.iter(|| black_box(parse_all(black_box(&source))));
    });
}

fn bench_plain_text_7kb(c: &mut Criterion) {
    c.bench_function("plain_text_7kb", |b| {
        b.iter(|| black_box(plain_text(black_box(RELEASE_EN))));
    });
}

fn bench_build_7kb(c: &mut Criterion) {
    let mut page_fixture = fixture(true);
    report(&mut page_fixture, "build_7kb", RELEASE_EN);
    let mut root_fixture = fixture(false);
    report(
        &mut root_fixture,
        "build_7kb_window_root_context",
        RELEASE_EN,
    );
    c.bench_function("build_7kb", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let mut node = mount(&mut page_fixture, RELEASE_EN);
                elapsed += start.elapsed();
                black_box(node.root());
                node.release(page_fixture.app.app_mut());
            }
            elapsed
        });
    });
}

fn bench_build_notice_html_525b(c: &mut Criterion) {
    let source = notice_source();
    let mut fixture = fixture(true);
    report(&mut fixture, "build_notice_html_525b", &source);
    c.bench_function("build_notice_html_525b", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let mut node = mount(&mut fixture, &source);
                elapsed += start.elapsed();
                black_box(node.root());
                node.release(fixture.app.app_mut());
            }
            elapsed
        });
    });
}

fn bench_frame_after_mount(c: &mut Criterion) {
    let mut fixture = fixture(true);
    let mut reporter = mount(&mut fixture, RELEASE_EN);
    let baseline = reset_allocations();
    let stats = fixture.app.drain().expect("first frame").stats;
    let allocations = allocation_report(baseline);
    let metrics = Metrics::collect(&fixture.app, reporter.root(), RELEASE_EN, allocations);
    eprintln!(
        "frame_after_mount: source_bytes={} shaped={} laid={} primitives={} instances={} {metrics}",
        RELEASE_EN.len(),
        stats.text.map_or(0, |report| report.shaped),
        stats.text_layout.map_or(0, |report| report.laid),
        stats.primitives,
        stats.instances
    );
    reporter.release(fixture.app.app_mut());
    c.bench_function("frame_after_mount", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let mut node = mount(&mut fixture, RELEASE_EN);
                let start = Instant::now();
                let stats = fixture.app.drain().expect("first frame").stats;
                elapsed += start.elapsed();
                black_box(stats);
                node.release(fixture.app.app_mut());
            }
            elapsed
        });
    });
}

fn bench_theme_rebuild_7kb(c: &mut Criterion) {
    let mut fixture = fixture(true);
    let mut reporter = mount(&mut fixture, RELEASE_EN);
    let _ = fixture.app.drain().expect("first frame");
    fixture
        .theme
        .set_appearance(fixture.app.app_mut(), Appearance::Dark);
    let baseline = reset_allocations();
    let rebuilt = reporter.sync(fixture.app.app_mut());
    let allocations = allocation_report(baseline);
    let _ = fixture.app.drain().expect("rebuilt frame");
    let metrics = Metrics::collect(&fixture.app, reporter.root(), RELEASE_EN, allocations);
    eprintln!("theme_rebuild_7kb: sync={rebuilt} {metrics}");
    reporter.release(fixture.app.app_mut());
    let mut toggle = true;
    c.bench_function("theme_rebuild_7kb", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let mut node = mount(&mut fixture, RELEASE_EN);
                let appearance = if toggle {
                    Appearance::Dark
                } else {
                    Appearance::Light
                };
                toggle = !toggle;
                fixture
                    .theme
                    .set_appearance(fixture.app.app_mut(), appearance);
                let start = Instant::now();
                black_box(node.sync(fixture.app.app_mut()));
                elapsed += start.elapsed();
                node.release(fixture.app.app_mut());
            }
            elapsed
        });
    });
}

fn bench_build_100kb(c: &mut Criterion) {
    let source = synthetic_markdown();
    let mut fixture = fixture(true);
    report(&mut fixture, "build_100kb", &source);
    c.bench_function("build_100kb", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let mut node = mount(&mut fixture, &source);
                elapsed += start.elapsed();
                black_box(node.root());
                node.release(fixture.app.app_mut());
            }
            elapsed
        });
    });
}

fn bench_build_table_10k(c: &mut Criterion) {
    let source = table_source(TABLE_ROWS, TABLE_COLUMNS);
    let mut fixture = fixture(true);
    report(&mut fixture, "build_table_10k", &source);
    eprintln!(
        "build_table_10k: rows={TABLE_ROWS} columns={TABLE_COLUMNS} source_cells={}",
        (TABLE_ROWS + 1) * TABLE_COLUMNS
    );
    c.bench_function("build_table_10k", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let mut node = mount(&mut fixture, &source);
                elapsed += start.elapsed();
                black_box(node.root());
                node.release(fixture.app.app_mut());
            }
            elapsed
        });
    });
}

fn bench_table_widths_200x20(c: &mut Criterion) {
    let source = table_source(WIDE_TABLE_ROWS, WIDE_TABLE_COLUMNS);
    let mut fixture = fixture(true);
    report(&mut fixture, "table_widths_200x20", &source);
    eprintln!(
        "table_widths_200x20: rows={WIDE_TABLE_ROWS} columns={WIDE_TABLE_COLUMNS} measured_cells={}",
        (WIDE_TABLE_ROWS + 1) * WIDE_TABLE_COLUMNS
    );
    c.bench_function("table_widths_200x20", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let mut node = mount(&mut fixture, &source);
                elapsed += start.elapsed();
                black_box(node.root());
                node.release(fixture.app.app_mut());
            }
            elapsed
        });
    });
}

criterion_group! {
    name = markdown_benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(2));
    targets =
        bench_parse_only_7kb,
        bench_parse_only_100kb,
        bench_plain_text_7kb,
        bench_build_7kb,
        bench_build_notice_html_525b,
        bench_frame_after_mount,
        bench_theme_rebuild_7kb,
        bench_build_100kb,
        bench_build_table_10k,
        bench_table_widths_200x20
}
criterion_main!(markdown_benches);

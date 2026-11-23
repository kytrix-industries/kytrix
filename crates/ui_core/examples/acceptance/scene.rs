use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    interaction::{ScrollConfig, ScrollHandle},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, Transform, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        primitive::Border,
        style::{Clip, PaintSpec, Style, StylePatch},
        style_builder::StyleBuilder,
    },
    text::{TextStyle, TextWrap},
    window::WindowId,
};

pub const WINDOW_WIDTH: LogicalPx = px(1280.0);
pub const WINDOW_HEIGHT: LogicalPx = px(800.0);
pub const WINDOW_SIZE: LogicalSize = LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT);

pub const BOOK_ROWS: usize = 5_000;
pub const BOOK_COLUMNS: usize = 8;
pub const TRADE_ROWS: usize = 500;
pub const TRADE_COLUMNS: usize = 4;
pub const CHART_POINTS: usize = 200_000;
pub const TEXT_UPDATE_ROWS: usize = 125;

pub const INCREMENTAL_ROWS: usize = 5;

const HEADER_HEIGHT: LogicalPx = px(44.0);
const SIDEBAR_WIDTH: LogicalPx = px(180.0);

const BOOK_RECT: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(SIDEBAR_WIDTH, HEADER_HEIGHT),
    LogicalSize::new(px(520.0), px(756.0)),
);
const CHART_RECT: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(px(700.0), HEADER_HEIGHT),
    LogicalSize::new(px(580.0), px(420.0)),
);
const TAPE_RECT: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(px(700.0), px(464.0)),
    LogicalSize::new(px(580.0), px(336.0)),
);

const BOOK_HEADER_HEIGHT: LogicalPx = px(44.0);
const TAPE_HEADER_HEIGHT: LogicalPx = px(36.0);
const BOOK_ROW_HEIGHT: LogicalPx = px(16.0);
const TRADE_ROW_HEIGHT: LogicalPx = px(16.0);
const BOOK_CELL_WIDTH: LogicalPx = px(65.0);
const TRADE_CELL_WIDTH: LogicalPx = px(145.0);
const TEXT_SIZE: LogicalPx = px(12.0);

const CHART_PLOT: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(
        px(CHART_RECT.origin.x.value() + 8.0),
        px(CHART_RECT.origin.y.value() + 34.0),
    ),
    LogicalSize::new(
        px(CHART_RECT.size.width.value() - 16.0),
        px(CHART_RECT.size.height.value() - 44.0),
    ),
);

const BG_RGBA: [u8; 4] = [0x0B, 0x0E, 0x13, 0xFF];
const CARD_RGBA: [u8; 4] = [0x14, 0x1A, 0x22, 0xFF];
const CARD_BORDER_RGBA: [u8; 4] = [0x22, 0x2C, 0x38, 0xFF];
const TEXT_RGBA: [u8; 4] = [0xE6, 0xE8, 0xEB, 0xFF];
const MUTED_RGBA: [u8; 4] = [0x93, 0xA4, 0xB5, 0xFF];
const ACCENT_RGBA: [u8; 4] = [0x4C, 0x9A, 0xFF, 0xFF];
const CHART_RGBA: [u8; 4] = [0xF0, 0xB4, 0x29, 0xFF];

#[cfg(windows)]
const MONO_FAMILY: &str = "Consolas";
#[cfg(target_os = "macos")]
const MONO_FAMILY: &str = "Menlo";
#[cfg(target_os = "linux")]
const MONO_FAMILY: &str = "DejaVu Sans Mono";

const BOOK_COLUMN_NAMES: [&str; BOOK_COLUMNS] = [
    "价格", "数量", "累计", "买量", "卖量", "笔数", "占比", "涨跌",
];
const TRADE_COLUMN_NAMES: [&str; TRADE_COLUMNS] = ["时间", "价格", "数量", "方向"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookRow(pub [u32; BOOK_COLUMNS]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TradeRow(pub u32);

struct Handles {
    chart: NodeId,
    book_viewport: NodeId,
    tape_viewport: NodeId,
}

pub struct Load {
    pub window: WindowId,
    book_rows: Vec<Signal<BookRow>>,
    trade_rows: Vec<Signal<TradeRow>>,
    pub chart: NodeId,
    tape_scroll: ScrollHandle,
    chart_pan: LogicalPx,
    step: u32,
    trade_seq: u32,
}

impl Load {
    pub fn build(app: &mut App, window: WindowId) -> Result<Self, EngineError> {
        let root = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree()
            .root();

        let mut book_rows = Vec::with_capacity(BOOK_ROWS);
        for row in 0..BOOK_ROWS {
            let values = std::array::from_fn(|column| book_value(row, column, 0));
            book_rows.push(app.signal(BookRow(values)));
        }
        let mut trade_rows = Vec::with_capacity(TRADE_ROWS);
        for row in 0..TRADE_ROWS {
            trade_rows.push(app.signal(TradeRow(row as u32)));
        }
        let chart_geometry = register_chart(app)?;

        let handles = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            build_tree(tree, root, &book_rows, &trade_rows, chart_geometry)
        };

        let book_scroll =
            app.make_scroll(window, handles.book_viewport, ScrollConfig::default())?;
        app.set_scroll_content(
            &book_scroll,
            LogicalSize::new(
                BOOK_RECT.size.width - 4.0,
                BOOK_ROW_HEIGHT * BOOK_ROWS as f32,
            ),
        )?;
        let tape_scroll =
            app.make_scroll(window, handles.tape_viewport, ScrollConfig::default())?;
        app.set_scroll_content(
            &tape_scroll,
            LogicalSize::new(
                TAPE_RECT.size.width - 4.0,
                TRADE_ROW_HEIGHT * TRADE_ROWS as f32,
            ),
        )?;

        Ok(Self {
            window,
            book_rows,
            trade_rows,
            chart: handles.chart,
            tape_scroll,
            chart_pan: LogicalPx::ZERO,
            step: 0,
            trade_seq: TRADE_ROWS as u32,
        })
    }

    #[must_use]
    pub fn node_count(&self, app: &App) -> usize {
        app.window(self.window)
            .map_or(0, |window| window.tree().len())
    }

    #[must_use]
    pub fn describe(&self, app: &App) -> String {
        format!(
            "负载：盘口 {BOOK_ROWS} 行 × {BOOK_COLUMNS} 列（{} 个数字文本）+ 成交 {TRADE_ROWS} 笔 × \
             {TRADE_COLUMNS} 列（{} 个文本）+ 折线 {CHART_POINTS} 点（几何，{} 顶点）+ 固定头部/侧栏；\
             窗口 {WINDOW_WIDTH:.0}×{WINDOW_HEIGHT:.0}；树内节点数 {}",
            BOOK_ROWS * BOOK_COLUMNS,
            TRADE_ROWS * TRADE_COLUMNS,
            CHART_POINTS * 2,
            self.node_count(app)
        )
    }

    pub fn incremental_step(&mut self, app: &mut App) {
        self.step = self.step.wrapping_add(1);
        for k in 0..INCREMENTAL_ROWS {
            let row = ((self.step as usize).wrapping_mul(37) + k * 971) % BOOK_ROWS;
            self.write_book_row(app, row);
        }
        self.append_trade(app);
        self.pan_chart(app);
    }

    pub fn text_update_step(&mut self, app: &mut App) {
        self.step = self.step.wrapping_add(1);
        for k in 0..TEXT_UPDATE_ROWS {
            let row = ((self.step as usize).wrapping_mul(13) + k * 37) % BOOK_ROWS;
            self.write_book_row(app, row);
        }
    }

    pub fn pan_step(&mut self, app: &mut App) {
        self.step = self.step.wrapping_add(1);
        self.pan_chart(app);
    }

    pub fn scroll_step(&mut self, app: &mut App) {
        self.step = self.step.wrapping_add(1);
        self.scroll_tape(app);
    }

    pub fn idle_step(&mut self) {
        self.step = self.step.wrapping_add(1);
    }

    #[must_use]
    pub fn step(&self) -> u32 {
        self.step
    }

    #[must_use]
    pub fn chart_pan(&self) -> LogicalPx {
        self.chart_pan
    }

    fn write_book_row(&mut self, app: &mut App, row: usize) {
        let values = std::array::from_fn(|column| book_value(row, column, self.step));
        let _ = app.write(&self.book_rows[row], BookRow(values));
    }

    fn append_trade(&mut self, app: &mut App) {
        let row = (self.trade_seq as usize) % TRADE_ROWS;
        self.trade_seq = self.trade_seq.wrapping_add(1);
        let _ = app.write(&self.trade_rows[row], TradeRow(self.trade_seq));
        self.scroll_tape(app);
    }

    fn scroll_tape(&mut self, app: &mut App) {
        let viewport = TAPE_RECT.size.height - TAPE_HEADER_HEIGHT - 4.0;
        let content = TRADE_ROW_HEIGHT * TRADE_ROWS as f32;
        let max = (content - viewport).max(px(1.0));
        let offset = px((TRADE_ROW_HEIGHT.value() * self.step as f32) % max.value());
        let _ = app.scroll_to(&self.tape_scroll, LogicalPoint::new(px(0.0), offset));
    }

    fn pan_chart(&mut self, app: &mut App) {
        self.chart_pan = px(f32::from((self.step % 16) as u8) * 0.5);
        let _ = app.set_geometry_transform(
            self.window,
            self.chart,
            Transform::translate(-self.chart_pan, px(0.0)),
        );
    }
}

fn build_tree(
    tree: &mut NodeTree,
    root: NodeId,
    book_rows: &[Signal<BookRow>],
    trade_rows: &[Signal<TradeRow>],
    chart_geometry: GeometryId,
) -> Handles {
    let card = Color::from_rgba8(CARD_RGBA);
    let border = Color::from_rgba8(CARD_BORDER_RGBA);
    let text_color = Color::from_rgba8(TEXT_RGBA);
    let muted = Color::from_rgba8(MUTED_RGBA);
    let accent = Color::from_rgba8(ACCENT_RGBA);

    let _ = NodeBuilder::boxed()
        .style(absolute_box(px(0.0), px(0.0), WINDOW_WIDTH, WINDOW_HEIGHT))
        .paint(PaintSpec {
            background: Color::from_rgba8(BG_RGBA),
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("背景");

    let header = NodeBuilder::boxed()
        .style(absolute_box(px(0.0), px(0.0), WINDOW_WIDTH, HEADER_HEIGHT))
        .paint(PaintSpec {
            background: card,
            border: Some(Border {
                width: px(1.0),
                color: border,
            }),
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("头部");
    label(
        tree,
        header,
        LogicalRect::new(px(16.0), px(6.0), px(560.0), px(20.0)),
        "KYTRIX · 验收负载：盘口 5000 行 + 成交 500 笔 + 20 万点折线 + 固定头部/侧栏",
        mono_style(px(13.0), text_color),
    );
    label(
        tree,
        header,
        LogicalRect::new(px(16.0), px(26.0), px(560.0), px(14.0)),
        "1280x800 · 增量帧：5 行盘口 + 1 笔成交 + 折线平移 0.5px",
        mono_style(px(11.0), muted),
    );
    for (index, content) in ["FPS  60.00", "FRAME  0.42ms", "UPLOAD  128B"]
        .into_iter()
        .enumerate()
    {
        label(
            tree,
            header,
            LogicalRect::new(
                px(1000.0) + px(94.0) * index as f32,
                px(14.0),
                px(92.0),
                px(16.0),
            ),
            content,
            mono_style(px(11.0), accent),
        );
    }

    let sidebar = NodeBuilder::boxed()
        .style(absolute_box(
            px(0.0),
            HEADER_HEIGHT,
            SIDEBAR_WIDTH,
            WINDOW_HEIGHT - HEADER_HEIGHT,
        ))
        .paint(PaintSpec {
            background: card,
            border: Some(Border {
                width: px(1.0),
                color: border,
            }),
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("侧栏");
    label(
        tree,
        sidebar,
        LogicalRect::new(px(12.0), px(10.0), px(150.0), px(18.0)),
        "导航",
        mono_style(px(12.0), text_color),
    );
    for index in 0..20usize {
        label(
            tree,
            sidebar,
            LogicalRect::new(
                px(12.0),
                px(36.0) + px(20.0) * index as f32,
                px(150.0),
                px(16.0),
            ),
            &format_smolstr!("菜单项 {:02}", index + 1),
            mono_style(px(11.0), muted),
        );
    }

    let book = panel(tree, root, BOOK_RECT, "盘口 · 5000 行 × 8 列（等宽数字）");
    for (column, name) in BOOK_COLUMN_NAMES.iter().enumerate() {
        label(
            tree,
            book,
            LogicalRect::new(
                px(6.0) + BOOK_CELL_WIDTH * column as f32,
                px(26.0),
                BOOK_CELL_WIDTH - 4.0,
                px(15.0),
            ),
            name,
            mono_style(px(11.0), muted),
        );
    }
    let book_viewport = NodeBuilder::boxed()
        .style(viewport_style(
            px(2.0),
            BOOK_HEADER_HEIGHT,
            BOOK_RECT.size.width - 4.0,
            BOOK_RECT.size.height - BOOK_HEADER_HEIGHT - 4.0,
        ))
        .build(tree, book)
        .expect("盘口视口");
    for (row, signal) in book_rows.iter().enumerate() {
        for column in 0..BOOK_COLUMNS {
            let source = *signal;
            let initial = book_text(book_value(row, column, 0), column);
            let _ = NodeBuilder::text(initial, mono_style(TEXT_SIZE, text_color))
                .layout_override(LogicalRect::new(
                    BOOK_CELL_WIDTH * column as f32,
                    BOOK_ROW_HEIGHT * row as f32,
                    BOOK_CELL_WIDTH - 4.0,
                    BOOK_ROW_HEIGHT - 2.0,
                ))
                .bind_text(move |cx| {
                    let row = cx.read(&source);
                    book_text(row.0[column], column)
                })
                .build(tree, book_viewport)
                .expect("盘口单元格");
        }
    }

    let chart_panel = panel(tree, root, CHART_RECT, "折线 · 200 000 点（静态几何通道）");
    label(
        tree,
        chart_panel,
        LogicalRect::new(px(10.0), px(22.0), px(320.0), px(14.0)),
        "每帧只改 per-draw 变换，顶点字节不重建",
        mono_style(px(10.0), muted),
    );
    let chart = NodeBuilder::geometry(chart_geometry)
        .layout_override(LogicalRect::new(
            CHART_PLOT.origin.x - CHART_RECT.origin.x,
            CHART_PLOT.origin.y - CHART_RECT.origin.y,
            CHART_PLOT.size.width,
            CHART_PLOT.size.height,
        ))
        .bind_style(|_| StylePatch {
            foreground: Some(Color::from_rgba8(CHART_RGBA)),
            ..StylePatch::default()
        })
        .build(tree, chart_panel)
        .expect("折线图节点");

    let tape = panel(tree, root, TAPE_RECT, "成交明细 · 500 笔（追加 + 滚动）");
    for (column, name) in TRADE_COLUMN_NAMES.iter().enumerate() {
        label(
            tree,
            tape,
            LogicalRect::new(
                px(6.0) + TRADE_CELL_WIDTH * column as f32,
                px(18.0),
                TRADE_CELL_WIDTH - 4.0,
                px(14.0),
            ),
            name,
            mono_style(px(11.0), muted),
        );
    }
    let tape_viewport = NodeBuilder::boxed()
        .style(viewport_style(
            px(2.0),
            TAPE_HEADER_HEIGHT,
            TAPE_RECT.size.width - 4.0,
            TAPE_RECT.size.height - TAPE_HEADER_HEIGHT - 4.0,
        ))
        .build(tree, tape)
        .expect("成交视口");
    for (row, signal) in trade_rows.iter().enumerate() {
        for column in 0..TRADE_COLUMNS {
            let source = *signal;
            let initial = trade_text(row as u32, column);
            let _ = NodeBuilder::text(initial, mono_style(TEXT_SIZE, text_color))
                .layout_override(LogicalRect::new(
                    TRADE_CELL_WIDTH * column as f32,
                    TRADE_ROW_HEIGHT * row as f32,
                    TRADE_CELL_WIDTH - 4.0,
                    TRADE_ROW_HEIGHT - 2.0,
                ))
                .bind_text(move |cx| {
                    let trade = cx.read(&source);
                    trade_text(trade.0, column)
                })
                .build(tree, tape_viewport)
                .expect("成交单元格");
        }
    }

    Handles {
        chart,
        book_viewport,
        tape_viewport,
    }
}

fn panel(tree: &mut NodeTree, root: NodeId, rect: LogicalRect, title: &str) -> NodeId {
    let panel = NodeBuilder::boxed()
        .style(absolute_box(
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
        ))
        .paint(PaintSpec {
            background: Color::from_rgba8(CARD_RGBA),
            border: Some(Border {
                width: px(1.0),
                color: Color::from_rgba8(CARD_BORDER_RGBA),
            }),
            corner_radius: [px(6.0); 4],
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("验收面板");
    label(
        tree,
        panel,
        LogicalRect::new(px(10.0), px(4.0), rect.size.width - 20.0, px(18.0)),
        title,
        mono_style(TEXT_SIZE, Color::from_rgba8(TEXT_RGBA)),
    );
    panel
}

fn label(tree: &mut NodeTree, parent: NodeId, rect: LogicalRect, content: &str, style: TextStyle) {
    let _ = NodeBuilder::text(content, style)
        .layout_override(rect)
        .build(tree, parent)
        .expect("验收标签");
}

#[must_use]
pub fn book_value(row: usize, column: usize, step: u32) -> u32 {
    let seed = (row as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (column as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    let base = ((seed ^ (seed >> 31) ^ (seed >> 17)) as u32).wrapping_add(step);
    match column {
        0 => 900_000 + base % 200_000,
        6 => 1 + base % 10_000,
        7 => 800 + base % 1_600,
        _ => 1 + base % 999_999,
    }
}

#[must_use]
pub fn book_text(value: u32, column: usize) -> SmolStr {
    match column {
        0 => format_smolstr!("{}.{:02}", value / 100, value % 100),
        6 => format_smolstr!("{}.{:02}%", value / 100, value % 100),
        7 => {
            let sign = if value & 0x8000_0000 != 0 { '-' } else { '+' };
            format_smolstr!("{sign}{}.{:02}", value / 100, value % 100)
        }
        _ => format_smolstr!("{value}"),
    }
}

#[must_use]
pub fn trade_text(seq: u32, column: usize) -> SmolStr {
    let price = 900_000 + seq.wrapping_mul(7_919) % 200_000;
    let size = 1 + seq.wrapping_mul(1_009) % 99_999;
    match column {
        0 => format_smolstr!("{:06}", seq % 1_000_000),
        1 => format_smolstr!("{}.{:02}", price / 100, price % 100),
        2 => format_smolstr!("{size}"),
        _ => SmolStr::new_static(if seq.is_multiple_of(2) { "B" } else { "S" }),
    }
}

fn mono_style(size: LogicalPx, color: Color) -> TextStyle {
    TextStyle::new(size)
        .with_family(MONO_FAMILY)
        .with_color(color)
        .with_wrap(TextWrap::NoWrap)
}

fn absolute_box(left: LogicalPx, top: LogicalPx, width: LogicalPx, height: LogicalPx) -> Style {
    StyleBuilder::new()
        .absolute()
        .left(left)
        .top(top)
        .size(width, height)
        .build()
}

fn viewport_style(left: LogicalPx, top: LogicalPx, width: LogicalPx, height: LogicalPx) -> Style {
    let mut style = absolute_box(left, top, width, height);
    style.clip = Clip::Bounds;
    style
}

fn register_chart(app: &mut App) -> Result<GeometryId, EngineError> {
    const VERTEX_BYTES: usize = 12;
    const HALF_THICKNESS: f32 = 0.5;
    const COVERAGE: f32 = -HALF_THICKNESS;

    let mut vertices = Vec::with_capacity(CHART_POINTS * 2 * VERTEX_BYTES);
    let mut indices = Vec::with_capacity(CHART_POINTS.saturating_sub(1) * 6 * 4);
    let height = CHART_PLOT.size.height.value();
    let mut y = height * 0.5;
    let dx = CHART_PLOT.size.width.value() / (CHART_POINTS - 1) as f32;

    for point in 0..CHART_POINTS {
        let noise = book_value(point, 3, 0);
        let delta = (noise % 2_001) as f32 / 1_000.0 - 1.0;
        y = (y + delta * 1.8).clamp(0.0, height);
        let x = CHART_PLOT.origin.x.value() + point as f32 * dx;
        let center = CHART_PLOT.origin.y.value() + y;
        for offset in [-HALF_THICKNESS, HALF_THICKNESS] {
            vertices.extend_from_slice(&x.to_le_bytes());
            vertices.extend_from_slice(&(center + offset).to_le_bytes());
            vertices.extend_from_slice(&COVERAGE.to_le_bytes());
        }
        if point > 0 {
            let base = ((point - 1) * 2) as u32;
            let next = (point * 2) as u32;
            for index in [base, base + 1, next, next, base + 1, next + 1] {
                indices.extend_from_slice(&index.to_le_bytes());
            }
        }
    }

    app.geometry_mut().insert(GeometryDesc {
        vertex_bytes: bytes::Bytes::from(vertices),
        index_bytes: bytes::Bytes::from(indices),
        vertex_stride: VERTEX_BYTES as u32,
        index_type: IndexType::U32,
        topology: Topology::TriangleList,
        bounds: LogicalRect::new(
            CHART_PLOT.origin.x - 16.0,
            CHART_PLOT.origin.y - 16.0,
            CHART_PLOT.size.width + 32.0,
            CHART_PLOT.size.height + 32.0,
        ),
        pipeline: GeometryPipeline::Ui,
    })
}

//! 最终验收的**固定合成负载**（`.docs/ui_core-整改计划.md` 的验收负载定义）。
//!
//! 单窗口 1280×800，三平台同一负载、同一更新脚本：
//!
//! | 区域 | 规模 | 更新方式 | 走哪条通道 |
//! |---|---|---|---|
//! | 盘口 | 5000 行 × 8 列数字文本（40 000 个文本节点） | 每增量帧只改 5 行 | 文本塑形 + 增量 lane 编码 |
//! | 成交明细 | 500 笔 × 4 列（2 000 个文本节点） | 每帧追加 1 笔 + 列表滚动 1 行 | 文本 + **滚动偏移**（不重建 lane） |
//! | 折线图 | 200 000 点（40 万顶点 / 120 万索引的静态几何） | 每帧平移窗口 0.5 px | 几何通道的 per-draw 变换（顶点不重建） |
//! | 头部 / 侧栏 | 固定文本与面板底色 | 不更新 | lane 整段复用 |
//!
//! 数字全部落在 `DigitFastPath` 的字符集里（`0-9 . , + - %`，见
//! `text/fast_path.rs::ALPHABET`），因此「等宽数字字体路径」被真实覆盖：盘口的每一格都是
//! 数字串，命中快速路径后跳过 parley 的 itemize/shaping。
//!
//! 所有位置都是**绝对定位**（`LayoutOverride` / 绝对 `Style`）：表格与看盘类界面的真实做法，
//! 也让「增量帧 CPU」不被 taffy 的整树布局淹没。固定头部/侧栏不在任何滚动容器里，
//! 它们的存在用来验证滚动帧不会波及不相关子树。

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    interaction::{ScrollConfig, ScrollHandle},
    math::{Point, Rect, Size, Transform},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        primitive::Border,
        style::{Clip, PaintSpec, Style, StylePatch},
    },
    text::{TextStyle, TextWrap},
    window::WindowId,
};

// ── 窗口与布局（写进报告，三平台一致） ──────────────────────────────────────

/// 验收窗口的逻辑宽度（`.docs/ui_core-整改计划.md`：单窗口 1280×800）。
pub const WINDOW_WIDTH: f32 = 1280.0;
/// 验收窗口的逻辑高度。
pub const WINDOW_HEIGHT: f32 = 800.0;
/// 验收窗口的逻辑尺寸。
pub const WINDOW_SIZE: Size = Size::new(WINDOW_WIDTH, WINDOW_HEIGHT);

/// 盘口行数。
pub const BOOK_ROWS: usize = 5_000;
/// 盘口列数（每行 8 个数字文本节点）。
pub const BOOK_COLUMNS: usize = 8;
/// 成交明细行数。
pub const TRADE_ROWS: usize = 500;
/// 成交明细列数。
pub const TRADE_COLUMNS: usize = 4;
/// 折线图的点数。
pub const CHART_POINTS: usize = 200_000;
/// 「1000 单元文本同时更新」的行数（125 × 8 = 1000 个文本节点）。
pub const TEXT_UPDATE_ROWS: usize = 125;

/// 每增量帧改写的盘口行数。
pub const INCREMENTAL_ROWS: usize = 5;

/// 头部高度（固定，不在滚动容器里）。
const HEADER_HEIGHT: f32 = 44.0;
/// 侧栏宽度（固定，不在滚动容器里）。
const SIDEBAR_WIDTH: f32 = 180.0;

/// 盘口面板矩形。
const BOOK_RECT: Rect = Rect::new(SIDEBAR_WIDTH, HEADER_HEIGHT, 520.0, 756.0);
/// 折线面板矩形。
const CHART_RECT: Rect = Rect::new(700.0, HEADER_HEIGHT, 580.0, 420.0);
/// 成交面板矩形。
const TAPE_RECT: Rect = Rect::new(700.0, 464.0, 580.0, 336.0);

/// 盘口面板内表头高度（标题 + 列名）。
const BOOK_HEADER_HEIGHT: f32 = 44.0;
/// 成交面板内表头高度。
const TAPE_HEADER_HEIGHT: f32 = 36.0;
/// 盘口行高。
const BOOK_ROW_HEIGHT: f32 = 16.0;
/// 成交行高。
const TRADE_ROW_HEIGHT: f32 = 16.0;
/// 盘口列宽。
const BOOK_CELL_WIDTH: f32 = 65.0;
/// 成交列宽。
const TRADE_CELL_WIDTH: f32 = 145.0;
/// 文本字号（盘口 / 成交 / 标签同一档；数字路径按字号缓存）。
const TEXT_SIZE: f32 = 12.0;

/// 折线图绘图区（**绝对内容坐标**；几何顶点与它同空间）。
const CHART_PLOT: Rect = Rect::new(
    CHART_RECT.origin.x + 8.0,
    CHART_RECT.origin.y + 34.0,
    CHART_RECT.size.width - 16.0,
    CHART_RECT.size.height - 44.0,
);

/// 背景色（`#0B0E13`）。
const BG_RGBA: [u8; 4] = [0x0B, 0x0E, 0x13, 0xFF];
/// 面板底色（`#141A22`）。
const CARD_RGBA: [u8; 4] = [0x14, 0x1A, 0x22, 0xFF];
/// 面板边框（`#222C38`）。
const CARD_BORDER_RGBA: [u8; 4] = [0x22, 0x2C, 0x38, 0xFF];
/// 正文色（`#E6E8EB`）。
const TEXT_RGBA: [u8; 4] = [0xE6, 0xE8, 0xEB, 0xFF];
/// 次要文字（`#93A4B5`）。
const MUTED_RGBA: [u8; 4] = [0x93, 0xA4, 0xB5, 0xFF];
/// 强调色（`#4C9AFF`）。
const ACCENT_RGBA: [u8; 4] = [0x4C, 0x9A, 0xFF, 0xFF];
/// 折线色（`#F0B429`）。
const CHART_RGBA: [u8; 4] = [0xF0, 0xB4, 0x29, 0xFF];

/// 平台等宽字体（数字表格的常规选择；缺失时由字体回退兜底）。
#[cfg(windows)]
const MONO_FAMILY: &str = "Consolas";
/// 平台等宽字体。
#[cfg(target_os = "macos")]
const MONO_FAMILY: &str = "Menlo";
/// 平台等宽字体。
#[cfg(target_os = "linux")]
const MONO_FAMILY: &str = "DejaVu Sans Mono";

/// 盘口列名（与 [`book_text`] 的列语义一一对应）。
const BOOK_COLUMN_NAMES: [&str; BOOK_COLUMNS] = [
    "价格", "数量", "累计", "买量", "卖量", "笔数", "占比", "涨跌",
];
/// 成交列名。
const TRADE_COLUMN_NAMES: [&str; TRADE_COLUMNS] = ["时间", "价格", "数量", "方向"];

// ── 场景句柄 ────────────────────────────────────────────────────────────────

/// 一行业务数据（8 列：价格分、数量、累计、买量、卖量、笔数、占比千分比、涨跌分）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookRow(pub [u32; BOOK_COLUMNS]);

/// 一笔成交（`seq` 是单调序号，其余列由它派生）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TradeRow(pub u32);

/// 建树期产出的句柄（拆出来是为了让整棵树在**一次** `&mut NodeTree` 借用里建完）。
struct Handles {
    chart: NodeId,
    book_viewport: NodeId,
    tape_viewport: NodeId,
}

/// 场景句柄：驱动更新需要的全部句柄与状态。
pub struct Load {
    /// 窗口。
    pub window: WindowId,
    /// 盘口每行一个信号（写 1 行 ⇒ 只失效该行的 8 个文本节点）。
    book_rows: Vec<Signal<BookRow>>,
    /// 成交每行一个信号。
    trade_rows: Vec<Signal<TradeRow>>,
    /// 折线图几何节点。
    pub chart: NodeId,
    /// 成交列表滚动句柄。
    tape_scroll: ScrollHandle,
    /// 折线窗口平移量（逻辑像素）。
    chart_pan: f32,
    /// 已推进的脚本步数（成交序号 / 值生成共用）。
    step: u32,
    /// 成交序号。
    trade_seq: u32,
}

impl Load {
    /// 挂载整份合成负载。
    ///
    /// # Errors
    /// 建树 / 几何注册 / 滚动容器创建失败（应用错误，验收脚本直接终止）。
    pub fn build(app: &mut App, window: WindowId) -> Result<Self, EngineError> {
        let root = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree()
            .root();

        // 信号与几何先建好：建树期间的 `&mut NodeTree` 借用不能再碰 `App`。
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
            Size::new(
                BOOK_RECT.size.width - 4.0,
                BOOK_ROWS as f32 * BOOK_ROW_HEIGHT,
            ),
        )?;
        let tape_scroll =
            app.make_scroll(window, handles.tape_viewport, ScrollConfig::default())?;
        app.set_scroll_content(
            &tape_scroll,
            Size::new(
                TAPE_RECT.size.width - 4.0,
                TRADE_ROWS as f32 * TRADE_ROW_HEIGHT,
            ),
        )?;

        Ok(Self {
            window,
            book_rows,
            trade_rows,
            chart: handles.chart,
            tape_scroll,
            chart_pan: 0.0,
            step: 0,
            trade_seq: TRADE_ROWS as u32,
        })
    }

    /// 场景的节点数（报告用）。
    #[must_use]
    pub fn node_count(&self, app: &App) -> usize {
        app.window(self.window)
            .map_or(0, |window| window.tree().len())
    }

    /// 一行负载描述（报告抬头；三平台必须一致）。
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

    /// 增量帧：改 5 行盘口 + 追加 1 笔成交并滚动 1 行 + 折线窗口平移 0.5 px。
    pub fn incremental_step(&mut self, app: &mut App) {
        self.step = self.step.wrapping_add(1);
        for k in 0..INCREMENTAL_ROWS {
            let row = ((self.step as usize).wrapping_mul(37) + k * 971) % BOOK_ROWS;
            self.write_book_row(app, row);
        }
        self.append_trade(app);
        self.pan_chart(app);
    }

    /// 「1000 单元文本同时更新」帧：125 行 × 8 列 = 1000 个文本节点全部换值。
    pub fn text_update_step(&mut self, app: &mut App) {
        self.step = self.step.wrapping_add(1);
        for k in 0..TEXT_UPDATE_ROWS {
            let row = ((self.step as usize).wrapping_mul(13) + k * 37) % BOOK_ROWS;
            self.write_book_row(app, row);
        }
    }

    /// 只平移折线窗口（隔离「几何通道 per-draw 变换」的成本）。
    pub fn pan_step(&mut self, app: &mut App) {
        self.step = self.step.wrapping_add(1);
        self.pan_chart(app);
    }

    /// 只滚动成交列表 1 行（隔离滚动偏移通道的成本）。
    pub fn scroll_step(&mut self, app: &mut App) {
        self.step = self.step.wrapping_add(1);
        self.scroll_tape(app);
    }

    /// 不写任何状态（空闲帧：帧门禁必须退化为 `Idle`）。
    pub fn idle_step(&mut self) {
        self.step = self.step.wrapping_add(1);
    }

    /// 当前脚本步数。
    #[must_use]
    pub fn step(&self) -> u32 {
        self.step
    }

    /// 折线窗口的当前平移量（逻辑像素）。
    #[must_use]
    pub fn chart_pan(&self) -> f32 {
        self.chart_pan
    }

    /// 把一行盘口的 8 列改成 `self.step` 对应的新值（值恒变 ⇒ 8 个文本节点必然失效）。
    fn write_book_row(&mut self, app: &mut App, row: usize) {
        let values = std::array::from_fn(|column| book_value(row, column, self.step));
        let _ = app.write(&self.book_rows[row], BookRow(values));
    }

    /// 追加一笔成交：写下一行的 4 个文本格，并把列表滚动 1 行。
    fn append_trade(&mut self, app: &mut App) {
        let row = (self.trade_seq as usize) % TRADE_ROWS;
        self.trade_seq = self.trade_seq.wrapping_add(1);
        let _ = app.write(&self.trade_rows[row], TradeRow(self.trade_seq));
        self.scroll_tape(app);
    }

    /// 成交列表滚动：每帧下移 1 行，到末尾回到顶部（数据是环形 500 行）。
    fn scroll_tape(&mut self, app: &mut App) {
        let viewport = TAPE_RECT.size.height - TAPE_HEADER_HEIGHT - 4.0;
        let content = TRADE_ROWS as f32 * TRADE_ROW_HEIGHT;
        let max = (content - viewport).max(1.0);
        let offset = (self.step as f32 * TRADE_ROW_HEIGHT) % max;
        let _ = app.scroll_to(&self.tape_scroll, Point::new(0.0, offset));
    }

    /// 折线窗口平移：0.5 px / 帧，0…7.5 px 循环（**只改 per-draw 变换**）。
    fn pan_chart(&mut self, app: &mut App) {
        self.chart_pan = f32::from((self.step % 16) as u8) * 0.5;
        let _ = app.set_geometry_transform(
            self.window,
            self.chart,
            Transform::translate(-self.chart_pan, 0.0),
        );
    }
}

// ── 建树 ────────────────────────────────────────────────────────────────────

/// 挂载全部节点（一次 `&mut NodeTree` 借用完成，建树期不再查窗口）。
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

    // 整窗背景（fixed，不随任何滚动移动）。
    let _ = NodeBuilder::boxed()
        .style(absolute_box(0.0, 0.0, WINDOW_WIDTH, WINDOW_HEIGHT))
        .paint(PaintSpec {
            background: Color::from_rgba8(BG_RGBA),
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("背景");

    // ── 固定头部 ─────────────────────────────────────────────────────────
    let header = NodeBuilder::boxed()
        .style(absolute_box(0.0, 0.0, WINDOW_WIDTH, HEADER_HEIGHT))
        .paint(PaintSpec {
            background: card,
            border: Some(Border {
                width: 1.0,
                color: border,
            }),
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("头部");
    label(
        tree,
        header,
        Rect::new(16.0, 6.0, 560.0, 20.0),
        "KYTRIX · 验收负载：盘口 5000 行 + 成交 500 笔 + 20 万点折线 + 固定头部/侧栏",
        mono_style(13.0, text_color),
    );
    label(
        tree,
        header,
        Rect::new(16.0, 26.0, 560.0, 14.0),
        "1280x800 · 增量帧：5 行盘口 + 1 笔成交 + 折线平移 0.5px",
        mono_style(11.0, muted),
    );
    for (index, content) in ["FPS  60.00", "FRAME  0.42ms", "UPLOAD  128B"]
        .into_iter()
        .enumerate()
    {
        label(
            tree,
            header,
            Rect::new(1000.0 + index as f32 * 94.0, 14.0, 92.0, 16.0),
            content,
            mono_style(11.0, accent),
        );
    }

    // ── 固定侧栏 ─────────────────────────────────────────────────────────
    let sidebar = NodeBuilder::boxed()
        .style(absolute_box(
            0.0,
            HEADER_HEIGHT,
            SIDEBAR_WIDTH,
            WINDOW_HEIGHT - HEADER_HEIGHT,
        ))
        .paint(PaintSpec {
            background: card,
            border: Some(Border {
                width: 1.0,
                color: border,
            }),
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("侧栏");
    label(
        tree,
        sidebar,
        Rect::new(12.0, 10.0, 150.0, 18.0),
        "导航",
        mono_style(12.0, text_color),
    );
    for index in 0..20usize {
        label(
            tree,
            sidebar,
            Rect::new(12.0, 36.0 + index as f32 * 20.0, 150.0, 16.0),
            &format_smolstr!("菜单项 {:02}", index + 1),
            mono_style(11.0, muted),
        );
    }

    // ── 盘口（5000 行 × 8 列） ───────────────────────────────────────────
    let book = panel(tree, root, BOOK_RECT, "盘口 · 5000 行 × 8 列（等宽数字）");
    for (column, name) in BOOK_COLUMN_NAMES.iter().enumerate() {
        label(
            tree,
            book,
            Rect::new(
                6.0 + column as f32 * BOOK_CELL_WIDTH,
                26.0,
                BOOK_CELL_WIDTH - 4.0,
                15.0,
            ),
            name,
            mono_style(11.0, muted),
        );
    }
    let book_viewport = NodeBuilder::boxed()
        .style(viewport_style(
            2.0,
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
                .layout_override(Rect::new(
                    column as f32 * BOOK_CELL_WIDTH,
                    row as f32 * BOOK_ROW_HEIGHT,
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

    // ── 折线图（20 万点静态几何） ────────────────────────────────────────
    let chart_panel = panel(tree, root, CHART_RECT, "折线 · 200 000 点（静态几何通道）");
    label(
        tree,
        chart_panel,
        Rect::new(10.0, 22.0, 320.0, 14.0),
        "每帧只改 per-draw 变换，顶点字节不重建",
        mono_style(10.0, muted),
    );
    let chart = NodeBuilder::geometry(chart_geometry)
        .layout_override(Rect::new(
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

    // ── 成交明细（500 笔 × 4 列） ────────────────────────────────────────
    let tape = panel(tree, root, TAPE_RECT, "成交明细 · 500 笔（追加 + 滚动）");
    for (column, name) in TRADE_COLUMN_NAMES.iter().enumerate() {
        label(
            tree,
            tape,
            Rect::new(
                6.0 + column as f32 * TRADE_CELL_WIDTH,
                18.0,
                TRADE_CELL_WIDTH - 4.0,
                14.0,
            ),
            name,
            mono_style(11.0, muted),
        );
    }
    let tape_viewport = NodeBuilder::boxed()
        .style(viewport_style(
            2.0,
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
                .layout_override(Rect::new(
                    column as f32 * TRADE_CELL_WIDTH,
                    row as f32 * TRADE_ROW_HEIGHT,
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

/// 面板：绝对定位 + 底色 + 边框 + 标题。
fn panel(tree: &mut NodeTree, root: NodeId, rect: Rect, title: &str) -> NodeId {
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
                width: 1.0,
                color: Color::from_rgba8(CARD_BORDER_RGBA),
            }),
            corner_radius: [6.0; 4],
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("验收面板");
    label(
        tree,
        panel,
        Rect::new(10.0, 4.0, rect.size.width - 20.0, 18.0),
        title,
        mono_style(TEXT_SIZE, Color::from_rgba8(TEXT_RGBA)),
    );
    panel
}

/// 绝对定位的文本叶子（标签 / 列名 / 表头）。
fn label(tree: &mut NodeTree, parent: NodeId, rect: Rect, content: &str, style: TextStyle) {
    let _ = NodeBuilder::text(content, style)
        .layout_override(rect)
        .build(tree, parent)
        .expect("验收标签");
}

// ── 值生成与文本格式（确定性：同一步在任何平台给出同一串） ──────────────────

/// 盘口单元格的值：列 0 是价格（分），列 6 是占比（千分比），列 7 是涨跌（分，带符号），
/// 其余是无量纲计数。
///
/// **每步必然变化**：取值 = 列内基准 + `step`，相邻两步的文本一定不同（否则
/// `App::write` 会退化成 no-op，那一帧少一条脏 lane，报告里的 `encoded_lanes` 就对不齐）。
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

/// 盘口单元格的文本（全部落在 `DigitFastPath` 字符集里）。
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

/// 成交单元格的文本。
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

// ── 建树小工具 ──────────────────────────────────────────────────────────────

/// 数字/表格文本样式：等宽族 + 不换行。
fn mono_style(size: f32, color: Color) -> TextStyle {
    TextStyle::new(size)
        .with_family(MONO_FAMILY)
        .with_color(color)
        .with_wrap(TextWrap::NoWrap)
}

/// 绝对定位盒（`left/top` 定死，`width/height` 定死；不参与父级 flex 流）。
fn absolute_box(left: f32, top: f32, width: f32, height: f32) -> Style {
    use taffy::{
        geometry::Rect as TaffyRect,
        style::{Dimension, LengthPercentageAuto, Position},
    };

    let mut style = Style::default();
    style.inner.position = Position::Absolute;
    style.inner.inset = TaffyRect {
        left: LengthPercentageAuto::length(left),
        top: LengthPercentageAuto::length(top),
        right: LengthPercentageAuto::auto(),
        bottom: LengthPercentageAuto::auto(),
    };
    style.inner.size = taffy::geometry::Size {
        width: Dimension::length(width),
        height: Dimension::length(height),
    };
    style
}

/// 滚动视口：绝对定位 + 裁剪到自身边框盒。
fn viewport_style(left: f32, top: f32, width: f32, height: f32) -> Style {
    let mut style = absolute_box(left, top, width, height);
    style.clip = Clip::Bounds;
    style
}

/// 注册 20 万点的折线几何（三角列表，2 顶点/点，6 索引/段）。
///
/// 顶点布局是 `GeometryPipeline::Ui` 的 12 B：`[x, y, coverage]`（逻辑像素 + 有符号覆盖率距离）。
/// 顶点用**绝对内容坐标**（与 `GeometryDesc::bounds` 同空间）；平移走 `App::set_geometry_transform`，
/// 顶点字节一次注册、永不重建。
///
/// # Errors
/// 几何注册校验失败（`GeometryRegister`）。
fn register_chart(app: &mut App) -> Result<GeometryId, EngineError> {
    /// `GeometryPipeline::Ui` 的每顶点字节数（位置 ×2 + 覆盖率距离）。
    const VERTEX_BYTES: usize = 12;
    /// 折线半厚度（逻辑像素）。
    const HALF_THICKNESS: f32 = 0.5;
    /// 覆盖率的距离字段：恒在半厚度内侧（形状内部 ⇒ 不透出 AA 带）。
    const COVERAGE: f32 = -HALF_THICKNESS;

    let mut vertices = Vec::with_capacity(CHART_POINTS * 2 * VERTEX_BYTES);
    let mut indices = Vec::with_capacity(CHART_POINTS.saturating_sub(1) * 6 * 4);
    let mut y = CHART_PLOT.size.height * 0.5;
    let dx = CHART_PLOT.size.width / (CHART_POINTS - 1) as f32;

    for point in 0..CHART_POINTS {
        // 确定性随机游走（无堆分配、无外部随机源；验收脚本要求同一步在平台间可复现）。
        let noise = book_value(point, 3, 0);
        let delta = (noise % 2_001) as f32 / 1_000.0 - 1.0;
        y = (y + delta * 1.8).clamp(0.0, CHART_PLOT.size.height);
        let x = CHART_PLOT.origin.x + point as f32 * dx;
        let center = CHART_PLOT.origin.y + y;
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
        // 窗口平移范围（0…7.5 px）留出余量：damage 取「变换前后 AABB 的并集」。
        bounds: Rect::new(
            CHART_PLOT.origin.x - 16.0,
            CHART_PLOT.origin.y - 16.0,
            CHART_PLOT.size.width + 32.0,
            CHART_PLOT.size.height + 32.0,
        ),
        pipeline: GeometryPipeline::Ui,
    })
}

//! 最终合成负载的**缩比夹具**与增量不变量（`cargo test --all-features` 门禁）。
//!
//! 全尺寸验收负载（盘口 5000 行 + 成交 500 笔 + 20 万点折线）由 example
//! `crates/ui_core/examples/acceptance` 驱动：CPU 计时必须用**生产特性**构建，否则
//! `test-support` 的逐帧 `FrameRecord` 采集会污染数字（见 `benches/perf.rs` 的同一说明）。
//! 本模块放同一负载的**缩比版**（[`Scale::MINI`]：300 行 × 8 列盘口 + 60 笔 × 4 列成交 +
//! 2 万点折线）与**与规模无关的确定性不变量**：
//!
//! 1. 增量帧只重编码「脏节点对应的 lane」，其余 lane 整段复用，且**每条 lane 都有归属**
//!    （`encoded_lanes + reused_segments` 恒等于首帧建立的总 lane 数）；
//! 2. 「1000 单元文本同时更新」帧恰好重编码 1000 条 lane；
//! 3. 滚动成交列表**不重编码任何 lane**（走滚动偏移通道），且偏移确实进了 delta；
//! 4. 折线窗口平移（`App::set_geometry_transform`）只重编码 1 条 lane，且**不上传实例字节**
//!    （顶点在几何通道里，per-draw 变换走 uniform）；
//! 5. 不写任何状态时帧门禁退化为 `Idle`（空闲零工作）。
//!
//! 这些断言与机器负载无关（计数是确定的），因此能进 CI 门禁；时间类指标只能在验收
//! harness 里量（数值受机器影响，见 `.review/最终验收报告.md`）。

use crate::{
    app::App,
    error::EngineError,
    interaction::{ScrollConfig, ScrollHandle},
    math::{Point, Rect, Size, Transform},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        style::{Clip, PaintSpec, Style},
    },
    text::{TextStyle, TextWrap},
    window::WindowId,
};

/// 缩比负载的规模（每行 8 列盘口 / 4 列成交是固定契约）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    /// 盘口行数。
    pub book_rows: usize,
    /// 成交行数。
    pub trade_rows: usize,
    /// 折线点数。
    pub chart_points: usize,
}

impl Scale {
    /// 门禁用缩比档（全尺寸的 1/17 左右，跑完 < 1 s）。
    pub const MINI: Self = Self {
        book_rows: 300,
        trade_rows: 60,
        chart_points: 20_000,
    };

    /// 盘口列数（每行 8 个数字文本节点）。
    pub const COLUMNS: usize = 8;
    /// 成交列数。
    pub const TRADE_COLUMNS: usize = 4;
    /// 「1000 单元」需要的盘口行数（125 × 8 = 1000 个文本节点）。
    pub const TEXT_ROWS: usize = 125;

    /// 每增量帧改写的行数。
    pub const INCREMENTAL_ROWS: usize = 3;

    /// 盘口视口逻辑高度（20 行可见）。
    pub const BOOK_VIEWPORT_HEIGHT: f32 = 320.0;
    /// 成交视口逻辑高度。
    pub const TAPE_VIEWPORT_HEIGHT: f32 = 160.0;
    /// 行高。
    pub const ROW_HEIGHT: f32 = 16.0;
    /// 列宽。
    pub const CELL_WIDTH: f32 = 64.0;
}

/// 一面盘口单元格的数据（列 0 是价格分，其余是计数）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookRow(pub [u32; Scale::COLUMNS]);

/// 缩比负载的句柄。
pub struct Load {
    /// 目标窗口。
    pub window: WindowId,
    /// 盘口每行一个信号。
    book_rows: Vec<Signal<BookRow>>,
    /// 成交每行一个信号。
    trade_rows: Vec<Signal<u32>>,
    /// 折线几何节点。
    pub chart: NodeId,
    /// 成交列表滚动句柄。
    tape_scroll: ScrollHandle,
    /// 盘口滚动句柄（本夹具不滚动它；留着证明滚动不波及别的容器）。
    pub book_scroll: ScrollHandle,
    /// 已推进步数。
    step: u32,
}

impl Load {
    /// 挂载缩比负载。
    ///
    /// # Errors
    /// 建树 / 几何注册 / 滚动容器创建失败。
    pub fn build(app: &mut App, window: WindowId, scale: Scale) -> Result<Self, EngineError> {
        assert!(scale.book_rows >= Scale::TEXT_ROWS, "缩比档至少要有 125 行盘口");
        let root = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree()
            .root();

        let mut book_rows = Vec::with_capacity(scale.book_rows);
        for row in 0..scale.book_rows {
            book_rows.push(app.signal(BookRow(std::array::from_fn(|column| {
                value(row, column, 0)
            }))));
        }
        let mut trade_rows = Vec::with_capacity(scale.trade_rows);
        for row in 0..scale.trade_rows {
            trade_rows.push(app.signal(row as u32));
        }
        let geometry = register_chart(app, scale.chart_points)?;

        let (book_viewport, tape_viewport, chart) = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            let book_viewport = NodeBuilder::boxed()
                .style(viewport_style(
                    0.0,
                    0.0,
                    scale.book_rows as f32 * Scale::CELL_WIDTH,
                    Scale::BOOK_VIEWPORT_HEIGHT,
                ))
                .build(tree, root)
                .expect("盘口视口");
            for (row, signal) in book_rows.iter().enumerate() {
                for column in 0..Scale::COLUMNS {
                    let source = *signal;
                    let _ = NodeBuilder::text(
                        text(value(row, column, 0), column),
                        cell_style(),
                    )
                    .layout_override(Rect::new(
                        column as f32 * Scale::CELL_WIDTH,
                        row as f32 * Scale::ROW_HEIGHT,
                        Scale::CELL_WIDTH - 2.0,
                        Scale::ROW_HEIGHT - 1.0,
                    ))
                    .bind_text(move |cx| {
                        let row = cx.read(&source);
                        text(row.0[column], column)
                    })
                    .build(tree, book_viewport)
                    .expect("盘口单元格");
                }
            }
            let tape_viewport = NodeBuilder::boxed()
                .style(viewport_style(
                    0.0,
                    Scale::BOOK_VIEWPORT_HEIGHT + 8.0,
                    scale.trade_rows as f32 * Scale::CELL_WIDTH,
                    Scale::TAPE_VIEWPORT_HEIGHT,
                ))
                .build(tree, root)
                .expect("成交视口");
            for (row, signal) in trade_rows.iter().enumerate() {
                for column in 0..Scale::TRADE_COLUMNS {
                    let source = *signal;
                    let _ = NodeBuilder::text(trade_text(row as u32, column), cell_style())
                        .layout_override(Rect::new(
                            column as f32 * Scale::CELL_WIDTH,
                            row as f32 * Scale::ROW_HEIGHT,
                            Scale::CELL_WIDTH - 2.0,
                            Scale::ROW_HEIGHT - 1.0,
                        ))
                        .bind_text(move |cx| trade_text(*cx.read(&source), column))
                        .build(tree, tape_viewport)
                        .expect("成交单元格");
                }
            }
            // 固定「头部」：不参与任何更新，用来验证增量帧不会波及静态子树。
            let _ = NodeBuilder::text("acceptance", cell_style())
                .layout_override(Rect::new(0.0, 0.0, 200.0, 20.0))
                .build(tree, root)
                .expect("固定头部");
            let chart = NodeBuilder::geometry(geometry)
                .layout_override(Rect::new(0.0, 0.0, 320.0, 200.0))
                .build(tree, root)
                .expect("折线节点");
            (book_viewport, tape_viewport, chart)
        };

        let book_scroll = app.make_scroll(window, book_viewport, ScrollConfig::default())?;
        app.set_scroll_content(
            &book_scroll,
            Size::new(
                scale.book_rows as f32 * Scale::CELL_WIDTH,
                scale.book_rows as f32 * Scale::ROW_HEIGHT,
            ),
        )?;
        let tape_scroll = app.make_scroll(window, tape_viewport, ScrollConfig::default())?;
        app.set_scroll_content(
            &tape_scroll,
            Size::new(
                scale.trade_rows as f32 * Scale::CELL_WIDTH,
                scale.trade_rows as f32 * Scale::ROW_HEIGHT,
            ),
        )?;

        Ok(Self {
            window,
            book_rows,
            trade_rows,
            chart,
            tape_scroll,
            book_scroll,
            step: 0,
        })
    }

    /// 增量帧：改 3 行盘口 + 追加 1 笔成交（写 4 格）+ 滚动 1 行 + 折线平移 0.5 px。
    pub fn incremental(&mut self, app: &mut App) {
        self.step += 1;
        for k in 0..Scale::INCREMENTAL_ROWS {
            let row = ((self.step as usize) * 37 + k * 97) % self.book_rows.len();
            self.write_book_row(app, row);
        }
        self.append_trade(app);
        self.scroll_tape(app);
        self.pan_chart(app);
    }

    /// 「1000 单元文本同时更新」帧：125 行 × 8 列。
    pub fn text_update(&mut self, app: &mut App) {
        self.step += 1;
        for k in 0..Scale::TEXT_ROWS {
            let row = ((self.step as usize) * 13 + k * 37) % self.book_rows.len();
            self.write_book_row(app, row);
        }
    }

    /// 只滚动成交列表。
    pub fn scroll(&mut self, app: &mut App) {
        self.step += 1;
        self.scroll_tape(app);
    }

    /// 只平移折线窗口。
    pub fn pan(&mut self, app: &mut App) {
        self.step += 1;
        self.pan_chart(app);
    }

    /// 每增量帧该重编码的 lane 数：3 行 × 8 列 + 4 个成交格 + 1 条几何 lane。
    #[must_use]
    pub const fn incremental_lanes() -> usize {
        Scale::INCREMENTAL_ROWS * Scale::COLUMNS + Scale::TRADE_COLUMNS + 1
    }

    fn write_book_row(&mut self, app: &mut App, row: usize) {
        let values = std::array::from_fn(|column| value(row, column, self.step));
        let _ = app.write(&self.book_rows[row], BookRow(values));
    }

    fn append_trade(&mut self, app: &mut App) {
        let row = (self.step as usize) % self.trade_rows.len();
        let _ = app.write(&self.trade_rows[row], self.step);
    }

    fn scroll_tape(&mut self, app: &mut App) {
        let content = self.trade_rows.len() as f32 * Scale::ROW_HEIGHT;
        let max = (content - Scale::TAPE_VIEWPORT_HEIGHT).max(Scale::ROW_HEIGHT);
        let offset = (self.step as f32 * Scale::ROW_HEIGHT) % max;
        let _ = app.scroll_to(&self.tape_scroll, Point::new(0.0, offset));
    }

    fn pan_chart(&mut self, app: &mut App) {
        let pan = (self.step % 16) as f32 * 0.5;
        let _ = app.set_geometry_transform(self.window, self.chart, Transform::translate(-pan, 0.0));
    }
}

/// 盘口单元格的值（列 0 价格分、列 7 涨跌带符号、其余计数）。
#[must_use]
pub fn value(row: usize, column: usize, step: u32) -> u32 {
    let seed = (row as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (column as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (step as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    let mixed = seed ^ (seed >> 31) ^ (seed >> 17);
    let value = mixed as u32;
    match column {
        0 => 9_000_00 + value % 2_000_00,
        7 => 8_00 + value % 1_600,
        _ => 1 + value % 999_999,
    }
}

/// 盘口单元格文本（全部落在 `DigitFastPath` 字符集里）。
#[must_use]
pub fn text(value: u32, column: usize) -> smol_str::SmolStr {
    use smol_str::format_smolstr;
    match column {
        0 => format_smolstr!("{}.{:02}", value / 100, value % 100),
        7 => {
            let sign = if value & 0x8000_0000 != 0 { '-' } else { '+' };
            format_smolstr!("{sign}{}.{:02}", value / 100, value % 100)
        }
        _ => format_smolstr!("{value}"),
    }
}

/// 成交单元格文本。
#[must_use]
pub fn trade_text(seq: u32, column: usize) -> smol_str::SmolStr {
    use smol_str::format_smolstr;
    match column {
        0 => format_smolstr!("{:06}", seq % 1_000_000),
        1 => format_smolstr!("{}.{:02}", (seq % 100_000) / 100, seq % 100),
        2 => format_smolstr!("{}", 1 + seq % 99_999),
        _ => smol_str::SmolStr::new_static(if seq.is_multiple_of(2) { "B" } else { "S" }),
    }
}

/// 单元格文本样式（等宽族 + 不换行）。
fn cell_style() -> TextStyle {
    #[cfg(windows)]
    const FAMILY: &str = "Consolas";
    #[cfg(target_os = "macos")]
    const FAMILY: &str = "Menlo";
    #[cfg(target_os = "linux")]
    const FAMILY: &str = "DejaVu Sans Mono";
    TextStyle::new(11.0)
        .with_family(FAMILY)
        .with_color(Color::WHITE)
        .with_wrap(TextWrap::NoWrap)
}

/// 绝对定位 + 裁剪到自身边框盒（滚动视口）。
fn viewport_style(left: f32, top: f32, width: f32, height: f32) -> Style {
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
    style.clip = Clip::Bounds;
    style
}

/// 注册缩比折线几何（三角列表，2 顶点/点，6 索引/段）。
///
/// # Errors
/// 几何注册校验失败（`GeometryRegister`）。
fn register_chart(app: &mut App, points: usize) -> Result<GeometryId, EngineError> {
    const VERTEX_BYTES: usize = 12;
    const HALF_THICKNESS: f32 = 0.5;

    let mut vertices = Vec::with_capacity(points * 2 * VERTEX_BYTES);
    let mut indices = Vec::with_capacity(points.saturating_sub(1) * 6 * 4);
    for point in 0..points {
        let x = (point % 320) as f32;
        let y = (point % 200) as f32;
        for offset in [-HALF_THICKNESS, HALF_THICKNESS] {
            vertices.extend_from_slice(&x.to_le_bytes());
            vertices.extend_from_slice(&(y + offset).to_le_bytes());
            vertices.extend_from_slice(&(-HALF_THICKNESS).to_le_bytes());
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
        bounds: Rect::new(0.0, 0.0, 320.0, 200.0),
        pipeline: GeometryPipeline::Ui,
    })
}

#[cfg(test)]
mod tests {
    use super::{Load, Scale};
    use crate::{
        app::App,
        math::{Scale as DpiScale, Size},
        test_support::record::FrameStats,
        window::{FrameOutcome, WindowOptions},
    };

    /// 无 GPU 的验收夹具：`App` 与它的 runtime 一起返回（`App` 只持句柄）。
    fn fixture() -> (tokio::runtime::Runtime, App, Load) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("acceptance-fixture"),
            size: Size::new(640.0, 480.0),
            scale: DpiScale::ONE,
            ..WindowOptions::default()
        });
        let load = Load::build(&mut app, window, Scale::MINI).expect("缩比负载");
        (runtime, app, load)
    }

    /// 提交一帧并要求它真的提交（空闲帧会让后续计数断言失去意义）。
    fn commit(app: &mut App, window: crate::window::WindowId) -> FrameStats {
        match app.frame(window).expect("窗口存在").expect("帧") {
            FrameOutcome::Committed(_) => *app
                .window(window)
                .and_then(crate::window::Window::frame_stats)
                .expect("提交帧必须有 FrameStats"),
            FrameOutcome::Idle => panic!("期望提交帧，实际是空闲帧"),
        }
    }

    /// 首帧建立总 lane 数（之后每帧的 `encoded_lanes + reused_segments` 必须恒等于它）。
    fn first_frame(app: &mut App, window: crate::window::WindowId) -> usize {
        let stats = commit(app, window);
        stats.encoded_lanes + stats.reused_segments
    }

    /// 增量帧：只有「3 行 × 8 列 + 4 个成交格 + 1 条几何 lane」被重编码，其余整段复用。
    #[test]
    fn incremental_frames_reencode_only_dirty_lanes() {
        let (_runtime, mut app, mut load) = fixture();
        let window = load.window;
        let total_lanes = first_frame(&mut app, window);
        assert_eq!(
            Load::incremental_lanes(),
            Scale::INCREMENTAL_ROWS * Scale::COLUMNS + Scale::TRADE_COLUMNS + 1,
            "脏 lane 数的口径必须写在夹具里（避免断言与实现同时漂移）"
        );

        for _ in 0..5 {
            load.incremental(&mut app);
            let stats = commit(&mut app, window);
            assert_eq!(
                stats.encoded_lanes,
                Load::incremental_lanes(),
                "增量帧只允许重编码脏 lane（实际 {}）—— 增量编码退化成全量？",
                stats.encoded_lanes
            );
            assert_eq!(
                stats.encoded_lanes + stats.reused_segments,
                total_lanes,
                "每条 lane 都必须有归属：重编码 {} + 复用 {} ≠ 总 lane {}",
                stats.encoded_lanes,
                stats.reused_segments,
                total_lanes
            );
            assert!(
                stats.uploaded_bytes > 0 && stats.uploaded_bytes < 60_000,
                "增量帧只允许上传脏 lane 的实例字节（实际 {} B；全量约 600 KB）",
                stats.uploaded_bytes
            );
        }
    }

    /// 「1000 单元文本同时更新」：恰好重编码 1000 条 lane（125 行 × 8 列）。
    #[test]
    fn a_thousand_cell_text_frame_reencodes_exactly_those_cells() {
        let (_runtime, mut app, mut load) = fixture();
        let window = load.window;
        let total_lanes = first_frame(&mut app, window);

        load.text_update(&mut app);
        let stats = commit(&mut app, window);
        assert_eq!(
            stats.encoded_lanes,
            Scale::TEXT_ROWS * Scale::COLUMNS,
            "1000 单元文本帧必须恰好重编码 1000 条 lane（实际 {}）",
            stats.encoded_lanes
        );
        assert_eq!(stats.encoded_lanes + stats.reused_segments, total_lanes);
        assert!(
            stats.uploaded_bytes >= 600_000 && stats.uploaded_bytes <= 1_400_000,
            "1000 个数字单元格的实例字节应在 0.6–1.4 MB（实际 {} B）",
            stats.uploaded_bytes
        );
    }

    /// 滚动帧：不重编码任何 lane、不上传实例字节，且偏移确实进了 delta。
    #[test]
    fn scrolling_the_tape_does_not_reencode_any_lane() {
        let (_runtime, mut app, mut load) = fixture();
        let window = load.window;
        let total_lanes = first_frame(&mut app, window);

        load.scroll(&mut app);
        let stats = commit(&mut app, window);
        assert_eq!(stats.encoded_lanes, 0, "滚动帧不得重编码任何 lane");
        assert_eq!(stats.reused_segments, total_lanes, "全部 lane 必须整段复用");
        assert_eq!(stats.uploaded_bytes, 0, "滚动帧不得上传实例字节");
        assert_eq!(
            stats.scroll_offset_frames, 1,
            "偏移变化必须进 delta（否则滚动会停在旧偏移上）"
        );
    }

    /// 折线平移：1 条几何 lane、0 实例字节（顶点在几何通道，变换走 uniform）。
    #[test]
    fn a_geometry_transform_frame_uploaded_no_instance_bytes() {
        let (_runtime, mut app, mut load) = fixture();
        let window = load.window;
        let _total_lanes = first_frame(&mut app, window);

        load.pan(&mut app);
        let stats = commit(&mut app, window);
        assert_eq!(
            stats.encoded_lanes, 1,
            "折线平移只允许重编码那 1 条几何 lane（实际 {}）",
            stats.encoded_lanes
        );
        assert_eq!(
            stats.uploaded_bytes, 0,
            "几何顶点不重建 ⇒ 不得上传实例字节（实际 {} B）",
            stats.uploaded_bytes
        );
    }

    /// 空闲帧：不写任何状态 ⇒ 帧门禁直接退化为 `Idle`（零工作、不推进提交游标）。
    #[test]
    fn an_idle_frame_is_a_zero_work_idle_frame() {
        let (_runtime, mut app, mut load) = fixture();
        let window = load.window;
        let _total_lanes = first_frame(&mut app, window);
        load.incremental(&mut app);
        let previous = commit(&mut app, window);
        assert!(
            previous.uploaded_bytes > 0,
            "上一个增量帧确实上传了实例字节（空闲帧不应重复上传）"
        );

        for _ in 0..3 {
            match app.frame(window).expect("窗口存在").expect("帧") {
                FrameOutcome::Idle => {}
                FrameOutcome::Committed(_) => {
                    panic!("没有脏位的帧必须退化为 Idle（实际提交了帧）")
                }
            }
        }
    }
}

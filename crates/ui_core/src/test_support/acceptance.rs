use crate::{
    app::App,
    error::EngineError,
    interaction::{ScrollConfig, ScrollHandle},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, Transform, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        atlas::AtlasKind,
        color::Color,
        geometry::{
            GeometryDesc, GeometryId, GeometryPipeline, GeometryUpdate, IndexType, Topology,
        },
        style::{Clip, Style},
        style_builder::StyleBuilder,
    },
    text::{TextStyle, TextWrap},
    window::WindowId,
};

const CHART_VERTEX_BYTES: usize = 12;
const CHART_HALF_THICKNESS: f32 = 0.5;
const HEATMAP_VERTEX_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    pub book_rows: usize,
    pub trade_rows: usize,
    pub chart_points: usize,
}

const HEATMAP_EDGE: usize = 32;

impl Scale {
    pub const MINI: Self = Self {
        book_rows: 300,
        trade_rows: 60,
        chart_points: 20_000,
    };

    pub const COLUMNS: usize = 8;
    pub const TRADE_COLUMNS: usize = 4;
    pub const TEXT_ROWS: usize = 125;

    pub const INCREMENTAL_ROWS: usize = 3;

    pub const BOOK_VIEWPORT_HEIGHT: f32 = 320.0;
    pub const TAPE_VIEWPORT_HEIGHT: f32 = 160.0;
    pub const ROW_HEIGHT: f32 = 16.0;
    pub const CELL_WIDTH: f32 = 64.0;
}

const TRADE_SEQ_START: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookRow(pub [u32; Scale::COLUMNS]);

pub struct Load {
    pub window: WindowId,
    book_rows: Vec<Signal<BookRow>>,
    trade_rows: Vec<Signal<u32>>,
    pub chart: NodeId,
    pub chart_geometry: GeometryId,
    pub heatmap: NodeId,
    pub heatmap_geometry: GeometryId,
    tape_scroll: ScrollHandle,
    pub book_scroll: ScrollHandle,
    step: u32,
}

impl Load {
    pub fn build(app: &mut App, window: WindowId, scale: Scale) -> Result<Self, EngineError> {
        assert!(
            scale.book_rows >= Scale::TEXT_ROWS,
            "缩比档至少要有 125 行盘口"
        );
        let root = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree()
            .root();

        let mut book_rows = Vec::with_capacity(scale.book_rows);
        for row in 0..scale.book_rows {
            book_rows
                .push(app.signal(BookRow(std::array::from_fn(|column| value(row, column, 0)))));
        }
        let mut trade_rows = Vec::with_capacity(scale.trade_rows);
        for row in 0..scale.trade_rows {
            trade_rows.push(app.signal(row as u32));
        }
        let geometry = register_chart(app, scale.chart_points)?;
        let heatmap_geometry = register_heatmap(app)?;

        let (book_viewport, tape_viewport, chart, heatmap) = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            let book_viewport = NodeBuilder::boxed()
                .style(viewport_style(
                    px(0.0),
                    px(0.0),
                    px(scale.book_rows as f32 * Scale::CELL_WIDTH),
                    px(Scale::BOOK_VIEWPORT_HEIGHT),
                ))
                .build(tree, root)
                .expect("盘口视口");
            for (row, signal) in book_rows.iter().enumerate() {
                for column in 0..Scale::COLUMNS {
                    let source = *signal;
                    let _ = NodeBuilder::text(text(value(row, column, 0), column), cell_style())
                        .layout_override(LogicalRect::new(
                            px(column as f32 * Scale::CELL_WIDTH),
                            px(row as f32 * Scale::ROW_HEIGHT),
                            px(Scale::CELL_WIDTH - 2.0),
                            px(Scale::ROW_HEIGHT - 1.0),
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
                    px(0.0),
                    px(Scale::BOOK_VIEWPORT_HEIGHT + 8.0),
                    px(scale.trade_rows as f32 * Scale::CELL_WIDTH),
                    px(Scale::TAPE_VIEWPORT_HEIGHT),
                ))
                .build(tree, root)
                .expect("成交视口");
            for (row, signal) in trade_rows.iter().enumerate() {
                for column in 0..Scale::TRADE_COLUMNS {
                    let source = *signal;
                    let _ = NodeBuilder::text(trade_text(row as u32, column), cell_style())
                        .layout_override(LogicalRect::new(
                            px(column as f32 * Scale::CELL_WIDTH),
                            px(row as f32 * Scale::ROW_HEIGHT),
                            px(Scale::CELL_WIDTH - 2.0),
                            px(Scale::ROW_HEIGHT - 1.0),
                        ))
                        .bind_text(move |cx| trade_text(*cx.read(&source), column))
                        .build(tree, tape_viewport)
                        .expect("成交单元格");
                }
            }
            let _ = NodeBuilder::text("acceptance", cell_style())
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(200.0), px(20.0)))
                .build(tree, root)
                .expect("固定头部");
            let chart = NodeBuilder::geometry(geometry)
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(320.0), px(200.0)))
                .build(tree, root)
                .expect("折线节点");
            let heatmap = NodeBuilder::geometry(heatmap_geometry)
                .layout_override(LogicalRect::new(px(328.0), px(0.0), px(256.0), px(256.0)))
                .build(tree, root)
                .expect("热力图节点");
            (book_viewport, tape_viewport, chart, heatmap)
        };

        let book_scroll = app.make_scroll(window, book_viewport, ScrollConfig::default())?;
        app.set_scroll_content(
            &book_scroll,
            LogicalSize::new(
                px(scale.book_rows as f32 * Scale::CELL_WIDTH),
                px(scale.book_rows as f32 * Scale::ROW_HEIGHT),
            ),
        )?;
        let tape_scroll = app.make_scroll(window, tape_viewport, ScrollConfig::default())?;
        app.set_scroll_content(
            &tape_scroll,
            LogicalSize::new(
                px(scale.trade_rows as f32 * Scale::CELL_WIDTH),
                px(scale.trade_rows as f32 * Scale::ROW_HEIGHT),
            ),
        )?;

        Ok(Self {
            window,
            book_rows,
            trade_rows,
            chart,
            chart_geometry: geometry,
            heatmap,
            heatmap_geometry,
            tape_scroll,
            book_scroll,
            step: 0,
        })
    }

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

    pub fn text_update(&mut self, app: &mut App) {
        self.step += 1;
        for k in 0..Scale::TEXT_ROWS {
            let row = ((self.step as usize) * 13 + k * 37) % self.book_rows.len();
            self.write_book_row(app, row);
        }
    }

    pub fn scroll(&mut self, app: &mut App) {
        self.step += 1;
        self.scroll_tape(app);
    }

    pub fn pan(&mut self, app: &mut App) {
        self.step += 1;
        self.pan_chart(app);
    }

    pub fn append_candle(&mut self, app: &mut App) -> Result<GeometryUpdate, EngineError> {
        self.step += 1;
        let snapshot = app.geometry().snapshot_arc(self.chart_geometry).ok_or(
            EngineError::UnknownGeometry {
                geometry: self.chart_geometry,
            },
        )?;
        let vertex_at = snapshot.vertex_len();
        let base = (vertex_at as usize / CHART_VERTEX_BYTES) as u32;
        let points = self.step as usize * 2 - 1;
        let x = (points % 320) as f32;
        let y = (points % 200) as f32;
        let mut vertices = Vec::with_capacity(2 * 12);
        for offset in [-CHART_HALF_THICKNESS, CHART_HALF_THICKNESS] {
            vertices.extend_from_slice(&x.to_ne_bytes());
            vertices.extend_from_slice(&(y + offset).to_ne_bytes());
            vertices.extend_from_slice(&(-CHART_HALF_THICKNESS).to_ne_bytes());
        }
        let update = app.update_geometry_vertices(
            self.chart_geometry,
            vertex_at..vertex_at + 24,
            &vertices,
        )?;
        let index_at = snapshot.index_len();
        let mut indices = Vec::with_capacity(24);
        let previous = base.saturating_sub(2);
        for index in [previous, previous + 1, base, base, previous + 1, base + 1] {
            indices.extend_from_slice(&index.to_ne_bytes());
        }
        app.update_geometry_indices(self.chart_geometry, index_at..index_at + 24, &indices)?;
        Ok(update)
    }

    pub fn heatmap_color_update(&mut self, app: &mut App) -> Result<GeometryUpdate, EngineError> {
        self.step += 1;
        let cell = (self.step as usize * 17) % (HEATMAP_EDGE * HEATMAP_EDGE);
        let offset = (cell * 4 * HEATMAP_VERTEX_BYTES) as u32;
        let shift = (self.step % 8) as f32 * 0.125;
        let mut vertices = Vec::with_capacity(4 * HEATMAP_VERTEX_BYTES);
        for (dx, dy) in [(0.0f32, 0.0f32), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            vertices.extend_from_slice(&dx.to_ne_bytes());
            vertices.extend_from_slice(&dy.to_ne_bytes());
            vertices.extend_from_slice(&(dx * 0.25 + shift).to_ne_bytes());
            vertices.extend_from_slice(&(dy * 0.25 + shift).to_ne_bytes());
        }
        app.update_geometry_vertices(
            self.heatmap_geometry,
            offset..offset + vertices.len() as u32,
            &vertices,
        )
    }

    #[must_use]
    pub fn incremental_lanes() -> usize {
        Scale::INCREMENTAL_ROWS * Scale::COLUMNS
            + Scale::TRADE_COLUMNS
            + chart_chunk_lanes(Scale::MINI.chart_points)
    }

    fn write_book_row(&mut self, app: &mut App, row: usize) {
        let values = std::array::from_fn(|column| value(row, column, self.step));
        let _ = app.write(&self.book_rows[row], BookRow(values));
    }

    fn append_trade(&mut self, app: &mut App) {
        let row = (self.step as usize) % self.trade_rows.len();
        let _ = app.write(&self.trade_rows[row], TRADE_SEQ_START + self.step);
    }

    fn scroll_tape(&mut self, app: &mut App) {
        let content = self.trade_rows.len() as f32 * Scale::ROW_HEIGHT;
        let max = (content - Scale::TAPE_VIEWPORT_HEIGHT).max(Scale::ROW_HEIGHT);
        let offset = (self.step as f32 * Scale::ROW_HEIGHT) % max;
        let _ = app.scroll_to(&self.tape_scroll, LogicalPoint::new(px(0.0), px(offset)));
    }

    fn pan_chart(&mut self, app: &mut App) {
        let pan = (self.step % 16) as f32 * 0.5;
        let _ = app.set_geometry_transform(
            self.window,
            self.chart,
            Transform::translate(px(-pan), px(0.0)),
        );
    }
}

fn chart_chunk_lanes(points: usize) -> usize {
    let indices = points.saturating_sub(1) * 6;
    indices.div_ceil(crate::paint::geometry::GEOMETRY_CHUNK_INDICES as usize)
}

#[must_use]
pub fn value(row: usize, column: usize, step: u32) -> u32 {
    let seed = (row as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (column as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    let base = ((seed ^ (seed >> 31) ^ (seed >> 17)) as u32).wrapping_add(step);
    match column {
        0 => 900_000 + base % 200_000,
        7 => 800 + base % 1_600,
        _ => 1 + base % 999_999,
    }
}

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

fn cell_style() -> TextStyle {
    #[cfg(windows)]
    const FAMILY: &str = "Consolas";
    #[cfg(target_os = "macos")]
    const FAMILY: &str = "Menlo";
    #[cfg(target_os = "linux")]
    const FAMILY: &str = "DejaVu Sans Mono";
    TextStyle::new(px(11.0))
        .with_family(FAMILY)
        .with_color(Color::WHITE)
        .with_wrap(TextWrap::NoWrap)
}

fn viewport_style(left: LogicalPx, top: LogicalPx, width: LogicalPx, height: LogicalPx) -> Style {
    let mut style = StyleBuilder::new()
        .absolute()
        .left(left)
        .top(top)
        .size(width, height)
        .build();
    style.clip = Clip::Bounds;
    style
}

fn register_chart(app: &mut App, points: usize) -> Result<GeometryId, EngineError> {
    let mut vertices = Vec::with_capacity(points * 2 * CHART_VERTEX_BYTES);
    let mut indices = Vec::with_capacity(points.saturating_sub(1) * 6 * 4);
    for point in 0..points {
        let x = (point % 320) as f32;
        let y = (point % 200) as f32;
        for offset in [-CHART_HALF_THICKNESS, CHART_HALF_THICKNESS] {
            vertices.extend_from_slice(&x.to_le_bytes());
            vertices.extend_from_slice(&(y + offset).to_le_bytes());
            vertices.extend_from_slice(&(-CHART_HALF_THICKNESS).to_le_bytes());
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
        vertex_stride: CHART_VERTEX_BYTES as u32,
        index_type: IndexType::U32,
        topology: Topology::TriangleList,
        bounds: LogicalRect::new(px(0.0), px(0.0), px(320.0), px(200.0)),
        pipeline: GeometryPipeline::Ui,
    })
}

fn register_heatmap(app: &mut App) -> Result<GeometryId, EngineError> {
    const EDGE: usize = 32;
    let cells = EDGE * EDGE;
    let mut vertices = Vec::with_capacity(cells * 4 * HEATMAP_VERTEX_BYTES);
    let mut indices = Vec::with_capacity(cells * 6 * 2);
    for row in 0..EDGE {
        for column in 0..EDGE {
            let cell = row * EDGE + column;
            let base = (cell * 4) as u16;
            let x = (column * 8) as f32;
            let y = (row * 8) as f32;
            for (dx, dy) in [(0.0f32, 0.0f32), (8.0, 0.0), (8.0, 8.0), (0.0, 8.0)] {
                vertices.extend_from_slice(&(x + dx).to_le_bytes());
                vertices.extend_from_slice(&(y + dy).to_le_bytes());
                vertices.extend_from_slice(&(dx / 8.0).to_le_bytes());
                vertices.extend_from_slice(&(dy / 8.0).to_le_bytes());
            }
            for index in [base, base + 1, base + 2, base, base + 2, base + 3] {
                indices.extend_from_slice(&index.to_le_bytes());
            }
        }
    }

    app.geometry_mut().insert(GeometryDesc {
        vertex_bytes: bytes::Bytes::from(vertices),
        index_bytes: bytes::Bytes::from(indices),
        vertex_stride: HEATMAP_VERTEX_BYTES as u32,
        index_type: IndexType::U16,
        topology: Topology::TriangleList,
        bounds: LogicalRect::new(px(328.0), px(0.0), px(256.0), px(256.0)),
        pipeline: GeometryPipeline::Textured {
            atlas: AtlasKind::Image,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::{Load, Scale, chart_chunk_lanes};
    use crate::{
        app::App,
        math::{LogicalSize, px},
        test_support::record::FrameStats,
        window::{FrameOutcome, WindowOptions},
    };

    fn fixture() -> (tokio::runtime::Runtime, App, Load) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("acceptance-fixture"),
            size: LogicalSize::new(px(640.0), px(480.0)),
            ..WindowOptions::default()
        });
        let load = Load::build(&mut app, window, Scale::MINI).expect("缩比负载");
        (runtime, app, load)
    }

    fn commit(app: &mut App, window: crate::window::WindowId) -> FrameStats {
        match app.frame(window).expect("窗口存在").expect("帧") {
            FrameOutcome::Committed(_) => *app
                .window(window)
                .and_then(crate::window::Window::frame_stats)
                .expect("提交帧必须有 FrameStats"),
            FrameOutcome::Idle => panic!("期望提交帧，实际是空闲帧"),
        }
    }

    fn first_frame(app: &mut App, window: crate::window::WindowId) -> usize {
        let stats = commit(app, window);
        stats.encoded_lanes + stats.reused_segments
    }

    #[test]
    fn incremental_frames_reencode_only_dirty_lanes() {
        let (_runtime, mut app, mut load) = fixture();
        let window = load.window;
        let total_lanes = first_frame(&mut app, window);
        assert_eq!(
            Load::incremental_lanes(),
            Scale::INCREMENTAL_ROWS * Scale::COLUMNS
                + Scale::TRADE_COLUMNS
                + chart_chunk_lanes(Scale::MINI.chart_points),
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

    #[test]
    fn a_geometry_transform_frame_uploaded_no_instance_bytes() {
        let (_runtime, mut app, mut load) = fixture();
        let window = load.window;
        let _total_lanes = first_frame(&mut app, window);
        let chunk_lanes = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .lane_count(load.chart);

        load.pan(&mut app);
        let stats = commit(&mut app, window);
        assert_eq!(
            stats.encoded_lanes, chunk_lanes,
            "折线平移只允许重编码这个几何节点的块 lane（{chunk_lanes} 条，实际 {}）",
            stats.encoded_lanes
        );
        assert_eq!(
            stats.geometry_encoded_chunks, chunk_lanes,
            "每条块 lane 恰好一条几何 op（实际 {}）",
            stats.geometry_encoded_chunks
        );
        assert_eq!(
            stats.uploaded_bytes, 0,
            "几何顶点不重建 ⇒ 不得上传实例字节（实际 {} B）",
            stats.uploaded_bytes
        );
        assert_eq!(
            stats.geometry_uploaded_bytes, 0,
            "只改 uniform 不触碰几何数据页 ⇒ GPU 几何上传 0 字节"
        );
    }

    #[test]
    fn appending_one_candle_rebuilds_and_uploads_only_the_tail_chunk() {
        let (_runtime, mut app, mut load) = fixture();
        let window = load.window;
        let _total = first_frame(&mut app, window);
        let chunk_lanes = chart_chunk_lanes(Scale::MINI.chart_points);
        assert!(
            chunk_lanes > 1,
            "折线必须跨多个几何块（否则测不到「只重建尾块」）"
        );
        let (tail_before, head_before, heatmap_before, heatmap_lanes) = {
            let state = app.window(window).expect("窗口存在");
            let tree = state.tree();
            assert_eq!(tree.lane_count(load.chart), chunk_lanes, "折线块 lane 数");
            let version = |node, chunk| {
                let lane = tree.lane_at(node, chunk).expect("块 lane");
                state.scene().lane_state(lane).expect("lane 状态").0
            };
            (
                version(load.chart, chunk_lanes - 1),
                version(load.chart, 0),
                version(load.heatmap, 0),
                tree.lane_count(load.heatmap),
            )
        };
        assert_eq!(heatmap_lanes, 1, "6144 索引的热力图是单个几何块");

        load.append_candle(&mut app).expect("追加一根 K 线");
        let stats = commit(&mut app, window);
        println!(
            "chart_append: encoded_lanes={} chunks={} instance_bytes={} geometry_bytes={}",
            stats.encoded_lanes,
            stats.geometry_encoded_chunks,
            stats.uploaded_bytes,
            stats.geometry_uploaded_bytes
        );
        assert_eq!(
            stats.encoded_lanes, 1,
            "只有尾块 lane 重建（实际 {}）",
            stats.encoded_lanes
        );
        assert_eq!(
            stats.geometry_encoded_chunks, 1,
            "只重编码尾块（实际 {}）",
            stats.geometry_encoded_chunks
        );
        assert_eq!(
            stats.uploaded_bytes, 0,
            "几何数据更新不动实例缓冲（实际 {} B）",
            stats.uploaded_bytes
        );
        assert!(
            stats.geometry_uploaded_bytes > 0
                && stats.geometry_uploaded_bytes <= 2 * crate::paint::geometry::GEOMETRY_PAGE_BYTES,
            "只上传尾块所在的顶点页 + 索引页（实际 {} B）",
            stats.geometry_uploaded_bytes
        );
        {
            let state = app.window(window).expect("窗口存在");
            let tree = state.tree();
            let version = |node, chunk| {
                let lane = tree.lane_at(node, chunk).expect("块 lane");
                state.scene().lane_state(lane).expect("lane 状态").0
            };
            assert!(
                version(load.chart, chunk_lanes - 1) > tail_before,
                "尾块 lane 必须重建"
            );
            assert_eq!(
                version(load.chart, 0),
                head_before,
                "非尾块 lane 不得被触碰"
            );
            assert_eq!(version(load.heatmap, 0), heatmap_before, "热力图不得被触碰");
        }
    }

    #[test]
    fn heatmap_single_cell_update_rebuilds_and_uploads_one_page() {
        let (_runtime, mut app, mut load) = fixture();
        let window = load.window;
        let _total = first_frame(&mut app, window);
        let (heatmap_before, chart_before) = {
            let state = app.window(window).expect("窗口存在");
            let tree = state.tree();
            let version = |node, chunk| {
                let lane = tree.lane_at(node, chunk).expect("块 lane");
                state.scene().lane_state(lane).expect("lane 状态").0
            };
            (version(load.heatmap, 0), version(load.chart, 0))
        };
        load.heatmap_color_update(&mut app).expect("改一格");
        let stats = commit(&mut app, window);
        println!(
            "heatmap_color_update: encoded_lanes={} chunks={} instance_bytes={} geometry_bytes={}",
            stats.encoded_lanes,
            stats.geometry_encoded_chunks,
            stats.uploaded_bytes,
            stats.geometry_uploaded_bytes
        );
        assert_eq!(stats.encoded_lanes, 1, "只有热力图唯一的块 lane 重建");
        assert_eq!(stats.geometry_encoded_chunks, 1);
        assert_eq!(stats.uploaded_bytes, 0, "不触碰实例缓冲");
        assert!(
            stats.geometry_uploaded_bytes > 0
                && stats.geometry_uploaded_bytes <= crate::paint::geometry::GEOMETRY_PAGE_BYTES,
            "只上传被改的那 1 个顶点页（实际 {} B）",
            stats.geometry_uploaded_bytes
        );
        {
            let state = app.window(window).expect("窗口存在");
            let tree = state.tree();
            let version = |node, chunk| {
                let lane = tree.lane_at(node, chunk).expect("块 lane");
                state.scene().lane_state(lane).expect("lane 状态").0
            };
            assert!(
                version(load.heatmap, 0) > heatmap_before,
                "热力图 lane 重建"
            );
            assert_eq!(version(load.chart, 0), chart_before, "折线不得被触碰");
        }
    }

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

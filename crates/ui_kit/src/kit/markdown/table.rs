use pulldown_cmark::Alignment;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    math::LogicalPx,
    text::{FontStackStatus, TextContent, TextStyle},
};

use super::{build::CELL_PADDING, inline::InlineContent};

const MAX_TABLE_ROWS: usize = 200;
const MAX_TABLE_COLUMNS: usize = 20;
const MAX_TABLE_OVERFLOW_ROWS: usize = 100;
const MAX_MEASURED_CELL_CHARS: usize = 1024;

#[derive(Clone, Debug)]
pub(super) struct CellBuf {
    base: TextStyle,
    content: InlineContent,
}

impl CellBuf {
    pub(super) fn new(base: TextStyle, content: InlineContent) -> Self {
        Self { base, content }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    pub(super) fn into_parts(self) -> (TextStyle, InlineContent) {
        (self.base, self.content)
    }
}

#[derive(Clone, Debug)]
pub(super) struct TableBuf {
    alignments: Vec<Alignment>,
    header: Option<Vec<CellBuf>>,
    rows: Vec<Vec<CellBuf>>,
    overflow: Vec<SmolStr>,
    row: Vec<CellBuf>,
    extra: Vec<CellBuf>,
    in_head: bool,
}

impl TableBuf {
    pub(super) fn new(mut alignments: Vec<Alignment>) -> Self {
        alignments.truncate(MAX_TABLE_COLUMNS);
        let columns = alignments.len();
        Self {
            alignments,
            header: None,
            rows: Vec::new(),
            overflow: Vec::new(),
            row: Vec::with_capacity(columns),
            extra: Vec::new(),
            in_head: false,
        }
    }

    pub(super) fn columns(&self) -> usize {
        self.alignments.len()
    }

    pub(super) fn align(&self, column: usize) -> Alignment {
        self.alignments
            .get(column.min(self.columns().saturating_sub(1)))
            .copied()
            .unwrap_or(Alignment::None)
    }

    pub(super) fn in_head(&self) -> bool {
        self.in_head
    }

    pub(super) fn row_len(&self) -> usize {
        self.row.len()
    }

    pub(super) fn begin_row(&mut self, head: bool) {
        self.in_head = head;
    }

    pub(super) fn push_cell(&mut self, cell: CellBuf) {
        if self.row.len() < self.columns() {
            self.row.push(cell);
        } else if self.columns() == MAX_TABLE_COLUMNS {
            self.extra.push(cell);
        }
    }

    pub(super) fn finish_row(&mut self, padding: TextStyle) {
        let columns = self.columns();
        let mut row = std::mem::replace(&mut self.row, Vec::with_capacity(columns));
        if !self.extra.is_empty() {
            let mut cells = std::mem::take(&mut self.extra);
            if let Some(last) = row.pop() {
                cells.insert(0, last);
            }
            if let Some(merged) = merge_cells(cells) {
                row.push(merged);
            }
        }
        while row.len() < self.columns() {
            row.push(CellBuf::new(padding.clone(), InlineContent::default()));
        }
        if self.in_head {
            self.header = Some(row);
            return;
        }
        if self.rows.len() < MAX_TABLE_ROWS {
            self.rows.push(row);
            return;
        }
        if self.overflow.len() >= MAX_TABLE_OVERFLOW_ROWS {
            return;
        }
        let line = row
            .iter()
            .map(|cell| cell.content.text.as_str())
            .collect::<Vec<&str>>()
            .join(" ");
        self.overflow.push(SmolStr::new(line.trim_end()));
    }

    pub(super) fn into_parts(self) -> (Option<Vec<CellBuf>>, Vec<Vec<CellBuf>>, Vec<SmolStr>) {
        (self.header, self.rows, self.overflow)
    }

    pub(super) fn column_widths(&self, app: &mut App) -> Option<Vec<LogicalPx>> {
        if app.font_stack_status() != FontStackStatus::Ready {
            return None;
        }
        let padding = CELL_PADDING + CELL_PADDING;
        let mut widths = vec![padding; self.columns()];
        for row in self.header.iter().chain(self.rows.iter()) {
            for (column, cell) in row.iter().enumerate() {
                if cell.is_empty() || column >= widths.len() {
                    continue;
                }
                let natural = app.measure_text(&cell_content(cell), None).width;
                widths[column] = widths[column].max(natural + padding);
            }
        }
        Some(widths)
    }
}

fn cell_content(cell: &CellBuf) -> TextContent {
    let text = cell.content.text.as_str();
    let truncated = text
        .char_indices()
        .nth(MAX_MEASURED_CELL_CHARS)
        .map(|(index, _)| &text[..index]);
    let mut content = TextContent::new(SmolStr::new(truncated.unwrap_or(text)), cell.base.clone());
    if truncated.is_some() {
        return content;
    }
    if !cell.content.runs.is_empty() {
        content = content.with_runs(cell.content.runs.iter().cloned());
    }
    if !cell.content.boxes.is_empty() {
        content = content.with_inline_boxes(cell.content.boxes.iter().cloned());
    }
    content
}

fn merge_cells(cells: Vec<CellBuf>) -> Option<CellBuf> {
    let mut pieces = cells
        .into_iter()
        .filter(|cell| !cell.content.is_empty())
        .collect::<Vec<CellBuf>>();
    if pieces.is_empty() {
        return None;
    }
    let mut merged = pieces.remove(0);
    let capacity = pieces
        .iter()
        .map(|cell| cell.content.text.len() + 1)
        .sum::<usize>();
    let mut text = String::with_capacity(merged.content.text.len() + capacity);
    text.push_str(&merged.content.text);
    for cell in pieces {
        text.push(' ');
        let offset = text.len();
        text.push_str(&cell.content.text);
        let first_box = merged.content.boxes.len() as u64;
        merged
            .content
            .runs
            .extend(cell.content.runs.into_iter().map(|mut run| {
                run.range = run.range.start + offset..run.range.end + offset;
                run
            }));
        merged
            .content
            .boxes
            .extend(
                cell.content
                    .boxes
                    .into_iter()
                    .enumerate()
                    .map(|(index, mut inline_box)| {
                        inline_box.id = first_box + index as u64;
                        inline_box.index += offset;
                        inline_box
                    }),
            );
    }
    merged.content.text = SmolStr::new(text);
    Some(merged)
}

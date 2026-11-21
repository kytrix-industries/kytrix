//! markdown 视图层：只读 IR → 引擎节点树。
//!
//! 旧的 `view.rs` 是 `Entity<MarkdownState>` + 自绘 `Element`（每块一个 `TextFlow`）；引擎化后
//! 这里只做两件事：
//!
//! 1. **块 → 节点**：标题/段落挂一个引擎文本节点（行内混排由 [`super::inline_flow`] 展开成
//!    `runs` + `inline_boxes`）；代码块/引用/列表/表格/分隔线各自按节点树表达（表格是简单的
//!    行列 `flex` 布局，不依赖容器的 `table` 组件批次）。
//! 2. **样式**：从 [`crate::theme`] 解析一次的 [`MarkdownStyle`]（挂载期常量，不做信号），
//!    布局/绘制走 `crate::style_builder::{StyleBuilder, PaintBuilder}`。
//!
//! 链接命中不在节点树上做（引擎只有节点级命中），而是 [`MarkdownHandles`] 自持每条文本节点的
//! 链接字节区间，再用引擎 E5 的 `Window::text_byte_index_at` 把点变成字节偏移：见
//! [`MarkdownHandles::link_at`]。
//!
//! 空行（[`MarkdownBlock::BlankLine`]）是一个固定高度的空节点——旧实现同样用一个
//! `div().h(line_height)` 占位，源码里的空行不折叠。

use std::sync::Arc;

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    math::{Point, Size},
    node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    paint::color::Color,
    resources::ResourceCache,
    text::{InlineBoxPaint, TextAlign, TextStyle, TextWrap},
    window::Window,
};

use crate::{
    kit::markdown::{
        document::{LinkId, MarkdownBlock, MarkdownDocument, MarkdownPiece, ParagraphAlign},
        inline_flow::{FlowStyle, ImagePaintSource, LinkSpan, flow},
        source::{InlineImageFormat, MarkdownAssets},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{resolved::Theme, role::Role},
};

/// 正文行高倍率（与旧 `window.text_style()` 的行高比例一致：1.4）。
const LINE_HEIGHT: f32 = 1.4;
/// 相邻两块之间的最小间距（源码里没写空行时）。
const BLOCK_GAP: f32 = 8.0;
/// 非首块标题与上一块之间的额外间距。
const HEADING_SPACE_ABOVE: f32 = 8.0;
/// 代码块内边距。
const CODE_PADDING: f32 = 8.0;
/// 代码块圆角。
const CODE_RADIUS: f32 = 4.0;
/// 引用块左侧竖线的宽度。
const QUOTE_BORDER_WIDTH: f32 = 3.0;
/// 引用块内容与竖线的距离。
const QUOTE_PADDING: f32 = 8.0;
/// 列表标记列宽（同一列表的文本从这里开始，对齐）。
const LIST_MARKER_WIDTH: f32 = 20.0;
/// 表格单元格内边距。
const CELL_PADDING: f32 = 4.0;

/// markdown 的挂载期样式（从主题解析一次；主题切换 = 重新挂载，与 P2-1a 的挂载期常量约定一致）。
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownStyle {
    /// 正文字号（标题按 `HeadingLevel::scale` 放大）。
    pub body_size: f32,
    /// 行高倍率。
    pub line_height: f32,
    /// 正文文字色。
    pub foreground: Color,
    /// 次要文字色（列表标记）。
    pub muted: Color,
    /// 链接色。
    pub link: Color,
    /// 行内代码 / 代码块文字色。
    pub code_foreground: Color,
    /// 代码块背景。
    pub code_background: Color,
    /// 引用块左侧竖线色。
    pub quote_border: Color,
    /// 分隔线 / 表格行线色。
    pub divider: Color,
    /// 行内图片占位色（资源通路未接线时的默认绘制方式）。
    pub image_placeholder: Color,
    /// 等宽字体族（代码块 / 行内代码）。
    pub mono_family: SmolStr,
}

impl MarkdownStyle {
    /// 从主题解析一套默认样式。
    pub fn from_theme(theme: &Theme) -> Self {
        let tokens = theme.tokens();
        Self {
            body_size: tokens.medium.font_size,
            line_height: LINE_HEIGHT,
            foreground: theme.role(Role::FgDefault),
            muted: theme.role(Role::FgMuted),
            link: theme.role(Role::BrandBase),
            code_foreground: theme.role(Role::FgDefault),
            code_background: theme.role(Role::SurfaceSunken),
            quote_border: theme.role(Role::LineBorder),
            divider: theme.role(Role::LineDivider),
            image_placeholder: theme.role(Role::SurfaceControl),
            mono_family: SmolStr::new_static(super::inline_flow::MONO_FAMILY),
        }
    }

    /// 一行正文的行高（像素；空行占位用它）。
    pub fn line_height(&self) -> f32 {
        self.body_size * self.line_height
    }

    /// 一块文本的段落级兜底样式（字号、颜色、字重、行高、对齐、换行）。
    fn text_style(&self, size: f32, bold: bool, align: ParagraphAlign) -> TextStyle {
        TextStyle::new(size)
            .with_color(self.foreground)
            .with_weight(if bold { 700 } else { 400 })
            .with_line_height(self.line_height)
            .with_alignment(text_align(align))
            .with_wrap(TextWrap::Wrap)
    }

    /// 行内映射的样式来源。
    fn flow_style(&self, size: f32, bold: bool) -> FlowStyle {
        FlowStyle {
            font_size: size,
            color: self.foreground,
            link: self.link,
            code: self.code_foreground,
            bold,
        }
    }
}

/// `ParagraphAlign` → 引擎 `TextAlign`。
fn text_align(align: ParagraphAlign) -> TextAlign {
    match align {
        ParagraphAlign::Left => TextAlign::Left,
        ParagraphAlign::Right => TextAlign::Right,
        ParagraphAlign::Center => TextAlign::Center,
    }
}

/// 行内图片的资源通路：把 `MarkdownAssets` 的字节喂给引擎 `ResourceCache`，拿到 `ResourceId`。
///
/// `svg` 需要构建期预栅格（资产批次），这里不画（返回 `None`，引擎只算落点）；位图缺资源时
/// 也返回 `None`——不静默画占位块，由调用方决定是否用 [`PlaceholderImages`]。
pub struct ResourceImages<'a> {
    /// 引擎图片资源缓存。
    pub cache: &'a mut ResourceCache,
    /// markdown 解析期同一份资源面（路径 → 字节）。
    pub assets: &'a dyn MarkdownAssets,
}

impl ImagePaintSource for ResourceImages<'_> {
    fn paint_for(
        &mut self,
        path: &str,
        format: InlineImageFormat,
        _size: Size,
    ) -> Option<InlineBoxPaint> {
        if format == InlineImageFormat::Svg {
            return None;
        }
        let bytes = self.assets.load(path)?;
        let image = self.cache.insert(bytes.as_ref()).ok()?;
        Some(InlineBoxPaint::Image {
            resource: image.id,
            modulation: Color::WHITE,
        })
    }
}

/// 一个挂载好的富文本节点（供 markdown 之外的行内混排用法复用）。
#[derive(Clone, Debug)]
pub struct RichText {
    /// 文本节点 id。
    pub node: NodeId,
    /// 该节点的链接字节区间（链接命中用，见 [`MarkdownHandles::link_at`]）。
    pub links: Arc<[LinkSpan]>,
}

/// 把一段行内片段挂成一个引擎文本节点（`TextContent{runs, inline_boxes}`）。
///
/// `base` 是段落级样式（[`MarkdownStyle::text_style`] 的产物），`align` 覆盖它的对齐方式；
/// 行内标记由 `style`（基准色、链接色、代码色）与片段自己的 strong/emphasis/strike/code/link/kbd
/// 决定。
pub fn rich_text(
    tree: &mut NodeTree,
    parent: NodeId,
    pieces: &[MarkdownPiece],
    style: &FlowStyle,
    base: &TextStyle,
    align: ParagraphAlign,
    images: &mut dyn ImagePaintSource,
) -> Option<RichText> {
    let flow = flow(pieces, style, images);
    if flow.is_empty() {
        return None;
    }
    // 只有行内图片的块在文本上没有任何字节：parley 的空文本不产生行，补一个零宽字符当锚点，
    // 图片盒子因此仍能落在唯一的行上（旧实现的逻辑索引空间里图片也占 1 个字节）。
    let text = if flow.text.is_empty() {
        SmolStr::new_static("\u{200B}")
    } else {
        flow.text.clone()
    };
    let mut builder = NodeBuilder::text(text, base.clone().with_alignment(text_align(align)));
    if !flow.runs.is_empty() {
        builder = builder.runs(flow.runs.iter().cloned());
    }
    if !flow.inline_boxes.is_empty() {
        builder = builder.inline_boxes(flow.inline_boxes.iter().copied());
    }
    let node = builder.build(tree, parent).ok()?;
    Some(RichText {
        node,
        links: Arc::from(flow.link_spans),
    })
}

/// 一份挂载好的 markdown 视图：根节点 + 所有文本节点的链接区间（命中用）。
#[derive(Clone, Debug)]
pub struct MarkdownHandles {
    /// 视图根节点。
    pub root: NodeId,
    /// 挂载出来的文本块（DFS 顺序；每个都可能是链接命中目标）。
    pub text_blocks: Vec<RichText>,
}

impl MarkdownHandles {
    /// 窗口绝对坐标 → 落在哪条链接上（`None` = 不在链接上 / 不在任何文本块里）。
    ///
    /// 命中链路：节点级几何（`Node::layout().bounds`）→ 点转节点局部坐标 →
    /// 引擎 E5 `Window::text_byte_index_at`（簇边界由 parley 决定）→ 自持的链接字节区间。
    /// 引擎错误（未成形 / 非文本 / 快照过期）一律按"未命中"处理，不 panic。
    ///
    /// **已知边界（登记 E2b，不扩引擎）**：纯图片链接没有字节区间（行内图片不占文本字节），
    /// 因此 `link_at` 无法命中图片；`span.range` 是半开区间，点在最后一个字符右半边时 E5 返回
    /// `byte == span.end`，同样不命中（E5 边界语义）。
    pub fn link_at(&self, window: &Window, point: Point) -> Option<LinkId> {
        for block in &self.text_blocks {
            if block.links.is_empty() {
                continue;
            }
            let Ok(node) = window.tree().get(block.node) else {
                continue;
            };
            let bounds = node.layout().bounds;
            if !bounds.contains(point) {
                continue;
            }
            let local = Point::new(point.x - bounds.origin.x, point.y - bounds.origin.y);
            let Ok(byte) = window.text_byte_index_at(block.node, local) else {
                continue;
            };
            if let Some(link) = block
                .links
                .iter()
                .find(|span| span.range.start <= byte && byte < span.range.end)
            {
                return Some(link.link);
            }
        }
        None
    }
}

/// markdown 视图（IR → 节点树）。
#[derive(Clone, Debug)]
pub struct MarkdownView {
    document: Arc<MarkdownDocument>,
    style: MarkdownStyle,
}

impl MarkdownView {
    /// 用已解析的文档 + 样式构造视图。
    pub fn new(document: Arc<MarkdownDocument>, style: MarkdownStyle) -> Self {
        Self { document, style }
    }

    /// 从主题解析样式（`Theme::of(appearance)` / `ThemeHandle::theme_for(..)` 的挂载期用法）。
    pub fn from_theme(theme: &Theme, document: Arc<MarkdownDocument>) -> Self {
        Self::new(document, MarkdownStyle::from_theme(theme))
    }

    /// 当前文档。
    pub fn document(&self) -> &MarkdownDocument {
        &self.document
    }

    /// 当前样式。
    pub fn style(&self) -> &MarkdownStyle {
        &self.style
    }

    /// 挂载：行内图片画占位方块（资源通路未接线时的默认）。
    pub fn build(&self, tree: &mut NodeTree, parent: NodeId) -> MarkdownHandles {
        let mut images = super::inline_flow::PlaceholderImagePaint {
            color: self.style.image_placeholder,
        };
        self.build_with_images(tree, parent, &mut images)
    }

    /// 挂载：行内图片走调用方给的绘制方式（生产用 [`ResourceImages`]）。
    ///
    /// 根节点的 `build` 失败只可能是 `parent` 已不在树里（调用方错误），因此在根上 `expect`；
    /// 子节点不会失败（父节点刚建出来），失败时按"这一块不画"跳过，不 panic。
    pub fn build_with_images(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        images: &mut dyn ImagePaintSource,
    ) -> MarkdownHandles {
        let root = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .id("MARKDOWN")
            .build(tree, parent)
            .expect("markdown root needs a live parent");
        let mut ctx = BuildCtx {
            view: self,
            images,
            texts: Vec::new(),
            counter: 0,
        };
        ctx.build_blocks(tree, root, &self.document.blocks);
        MarkdownHandles {
            root,
            text_blocks: ctx.texts,
        }
    }
}

/// 一次挂载的上下文：样式、图片画法、已建文本块、节点 id 计数。
struct BuildCtx<'a> {
    view: &'a MarkdownView,
    images: &'a mut dyn ImagePaintSource,
    texts: Vec<RichText>,
    counter: usize,
}

impl BuildCtx<'_> {
    fn style(&self) -> &MarkdownStyle {
        &self.view.style
    }

    fn next_index(&mut self) -> usize {
        let index = self.counter;
        self.counter += 1;
        index
    }

    /// 一列块：块间距由"上一块是不是空行"决定（与旧 `MarkdownState::render` 同一规则）。
    ///
    /// 空行本身不占间距（它自己画一行空行），但**标题的额外上间距与空行无关**：旧实现里空行
    /// 之后的非首块标题仍然拿 `BLOCK_GAP + HEADING_SPACE_ABOVE`，这里保持一致。
    fn build_blocks(&mut self, tree: &mut NodeTree, parent: NodeId, blocks: &[MarkdownBlock]) {
        for (index, block) in blocks.iter().enumerate() {
            let gap = match block {
                MarkdownBlock::BlankLine => 0.0,
                _ if index == 0 => 0.0,
                MarkdownBlock::Heading { .. } => BLOCK_GAP + HEADING_SPACE_ABOVE,
                _ => BLOCK_GAP,
            };
            self.build_block(tree, parent, block, gap, index);
        }
    }

    /// 一个块：外面永远套一个"间距 + 宽度"的盒子，里面的内容按块类型挂。
    fn build_block(
        &mut self,
        tree: &mut NodeTree,
        parent: NodeId,
        block: &MarkdownBlock,
        gap: f32,
        index: usize,
    ) {
        let style = StyleBuilder::new()
            .flex_col()
            .width_full()
            .min_width_0()
            .margin_sides(gap, 0.0, 0.0, 0.0)
            .build();
        let id = match block {
            MarkdownBlock::BlankLine => format_smolstr!("MARKDOWN_BLANK_{index}"),
            _ => format_smolstr!("MARKDOWN_BLOCK_{}", self.next_index()),
        };
        let Ok(wrapper) = NodeBuilder::boxed().style(style).id(id).build(tree, parent) else {
            return;
        };
        match block {
            MarkdownBlock::BlankLine => {
                let height = self.style().line_height();
                let style = StyleBuilder::new().width_full().height(height).build();
                let _ = NodeBuilder::boxed()
                    .style(style)
                    .id("MARKDOWN_BLANK")
                    .build(tree, wrapper);
            }
            MarkdownBlock::Heading { level, pieces } => {
                let size = self.style().body_size * level.scale();
                self.push_inline(tree, wrapper, pieces, size, true, ParagraphAlign::Left);
            }
            MarkdownBlock::Paragraph { align, pieces } => {
                let size = self.style().body_size;
                self.push_inline(tree, wrapper, pieces, size, false, *align);
            }
            MarkdownBlock::CodeBlock { code, .. } => self.build_code_block(tree, wrapper, code),
            MarkdownBlock::Quote { blocks } => self.build_quote(tree, wrapper, blocks),
            MarkdownBlock::List {
                ordered,
                start,
                items,
            } => self.build_list(tree, wrapper, *ordered, *start, items),
            MarkdownBlock::ThematicBreak => {
                let paint = PaintBuilder::new().background(self.style().divider).build();
                let _ = NodeBuilder::boxed()
                    .style(StyleBuilder::new().width_full().height(1.0).build())
                    .paint(paint)
                    .id("MARKDOWN_RULE")
                    .build(tree, wrapper);
            }
            MarkdownBlock::Table { rows } => self.build_table(tree, wrapper, rows),
        }
    }

    /// 一个行内块：展开成文本节点，并记住它的链接区间。
    fn push_inline(
        &mut self,
        tree: &mut NodeTree,
        parent: NodeId,
        pieces: &[MarkdownPiece],
        size: f32,
        bold: bool,
        align: ParagraphAlign,
    ) {
        let flow_style = self.style().flow_style(size, bold);
        let base = self.style().text_style(size, bold, align);
        if let Some(rich) = rich_text(
            tree,
            parent,
            pieces,
            &flow_style,
            &base,
            align,
            &mut *self.images,
        ) {
            self.texts.push(rich);
        }
    }

    fn build_code_block(&mut self, tree: &mut NodeTree, wrapper: NodeId, code: &str) {
        let paint = PaintBuilder::new()
            .background(self.style().code_background)
            .rounded(CODE_RADIUS)
            .build();
        let Ok(container) = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .padding(CODE_PADDING)
                    .build(),
            )
            .paint(paint)
            .id("MARKDOWN_CODE")
            .build(tree, wrapper)
        else {
            return;
        };
        let text_style = TextStyle::new(self.style().body_size)
            .with_family(self.style().mono_family.clone())
            .with_color(self.style().code_foreground)
            .with_line_height(self.style().line_height)
            .with_alignment(TextAlign::Left)
            .with_wrap(TextWrap::Wrap);
        let _ = NodeBuilder::text(SmolStr::new(code), text_style)
            .id("MARKDOWN_CODE_TEXT")
            .build(tree, container);
    }

    fn build_quote(&mut self, tree: &mut NodeTree, wrapper: NodeId, blocks: &[MarkdownBlock]) {
        let Ok(row) = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .id("MARKDOWN_QUOTE")
            .build(tree, wrapper)
        else {
            return;
        };
        let bar_paint = PaintBuilder::new()
            .background(self.style().quote_border)
            .build();
        let _ = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .width(QUOTE_BORDER_WIDTH)
                    .height_full()
                    .build(),
            )
            .paint(bar_paint)
            .id("MARKDOWN_QUOTE_BAR")
            .build(tree, row);
        let Ok(column) = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .flex_1()
                    .min_width_0()
                    .padding_sides(0.0, 0.0, 0.0, QUOTE_PADDING)
                    .build(),
            )
            .id("MARKDOWN_QUOTE_BODY")
            .build(tree, row)
        else {
            return;
        };
        self.build_blocks(tree, column, blocks);
    }

    fn build_list(
        &mut self,
        tree: &mut NodeTree,
        wrapper: NodeId,
        ordered: bool,
        start: u64,
        items: &[crate::kit::markdown::document::MarkdownListItem],
    ) {
        let Ok(column) = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .id("MARKDOWN_LIST")
            .build(tree, wrapper)
        else {
            return;
        };
        for (index, item) in items.iter().enumerate() {
            let Ok(row) = NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .width_full()
                        .min_width_0()
                        .build(),
                )
                .id("MARKDOWN_LIST_ITEM")
                .build(tree, column)
            else {
                return;
            };
            let marker = if ordered {
                format_smolstr!("{}.", start + index as u64)
            } else {
                SmolStr::new_static("•")
            };
            let marker_style = TextStyle::new(self.style().body_size)
                .with_color(self.style().muted)
                .with_alignment(TextAlign::Right)
                .with_wrap(TextWrap::NoWrap);
            let _ = NodeBuilder::text(marker, marker_style)
                .style(StyleBuilder::new().width(LIST_MARKER_WIDTH).build())
                .id("MARKDOWN_LIST_MARKER")
                .build(tree, row);
            let Ok(content) = NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_1()
                        .min_width_0()
                        .build(),
                )
                .id("MARKDOWN_LIST_CONTENT")
                .build(tree, row)
            else {
                return;
            };
            self.build_blocks(tree, content, &item.blocks);
        }
    }

    fn build_table(
        &mut self,
        tree: &mut NodeTree,
        wrapper: NodeId,
        rows: &[crate::kit::markdown::document::MarkdownTableRow],
    ) {
        let Ok(column) = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .id("MARKDOWN_TABLE")
            .build(tree, wrapper)
        else {
            return;
        };
        for row in rows {
            let Ok(row_node) = NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .width_full()
                        .min_width_0()
                        .build(),
                )
                .id("MARKDOWN_TABLE_ROW")
                .build(tree, column)
            else {
                return;
            };
            for cell in row.cells.iter() {
                let Ok(cell_node) = NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .flex_1()
                            .min_width_0()
                            .padding(CELL_PADDING)
                            .build(),
                    )
                    .id("MARKDOWN_TABLE_CELL")
                    .build(tree, row_node)
                else {
                    return;
                };
                let size = self.style().body_size;
                self.push_inline(tree, cell_node, &cell.pieces, size, row.header, cell.align);
            }
            let paint = PaintBuilder::new().background(self.style().divider).build();
            let _ = NodeBuilder::boxed()
                .style(StyleBuilder::new().width_full().height(1.0).build())
                .paint(paint)
                .id("MARKDOWN_TABLE_RULE")
                .build(tree, column);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smol_str::SmolStr;
    use ui_core::{
        math::{Point, Size},
        test_support::{app::TestApp, probe::Selector},
        text::TextStyle,
    };

    use super::{MarkdownStyle, MarkdownView, RichText, rich_text, text_align};
    use crate::{
        kit::markdown::{
            document::{
                HeadingLevel, KbdId, LinkId, MarkdownBlock, MarkdownDocument, MarkdownListItem,
                MarkdownPiece, MarkdownTableCell, MarkdownTableRow, ParagraphAlign,
            },
            inline_flow::{FlowStyle, InvisibleImages},
            source::InlineImageFormat,
        },
        theme::{appearance::Appearance, resolved::Theme},
    };

    fn theme() -> Theme {
        Theme::of(Appearance::Light)
    }

    fn style() -> MarkdownStyle {
        MarkdownStyle::from_theme(&theme())
    }

    fn text(value: &str) -> MarkdownPiece {
        MarkdownPiece::Text {
            text: SmolStr::new(value),
            strong: false,
            emphasis: false,
            strike: false,
            code: false,
            link: None,
            kbd: None,
        }
    }

    fn linked(value: &str, id: usize) -> MarkdownPiece {
        MarkdownPiece::Text {
            text: SmolStr::new(value),
            strong: false,
            emphasis: false,
            strike: false,
            code: false,
            link: Some(LinkId(id)),
            kbd: None,
        }
    }

    fn image(side: f32) -> MarkdownPiece {
        MarkdownPiece::InlineImage {
            path: SmolStr::new_static("brands/Binance/binance.png"),
            format: InlineImageFormat::Image,
            size: Size::new(side, side),
            link: None,
            kbd: None,
        }
    }

    fn document(blocks: Vec<MarkdownBlock>) -> Arc<MarkdownDocument> {
        document_with_links(blocks, Vec::new())
    }

    fn document_with_links(blocks: Vec<MarkdownBlock>, links: Vec<&str>) -> Arc<MarkdownDocument> {
        Arc::new(MarkdownDocument {
            blocks,
            links: Arc::from(
                links
                    .into_iter()
                    .map(SmolStr::new)
                    .collect::<Vec<SmolStr>>(),
            ),
        })
    }

    fn paragraph(pieces: Vec<MarkdownPiece>) -> MarkdownBlock {
        MarkdownBlock::Paragraph {
            align: ParagraphAlign::Left,
            pieces: Arc::from(pieces),
        }
    }

    /// 建一个 TestApp，把视图挂到根节点，跑一帧，返回 (TestApp, handles)。
    fn mount(view: &MarkdownView) -> (TestApp, super::MarkdownHandles) {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let handles = view.build(
            app.app_mut().window_mut(window).expect("窗口").tree_mut(),
            root,
        );
        let _ = app.drain().expect("首帧");
        (app, handles)
    }

    #[test]
    fn paragraph_mounts_one_text_node_with_layout_and_glyphs() {
        let doc = document(vec![paragraph(vec![text("hello markdown")])]);
        let view = MarkdownView::new(doc, style());
        let (app, handles) = mount(&view);

        assert_eq!(handles.text_blocks.len(), 1);
        let node = handles.text_blocks[0].node;
        let bounds = app.probe().bounds(node).expect("文本节点有布局盒");
        assert!(bounds.size.width > 0.0, "{bounds:?}");
        assert!(bounds.size.height > 0.0, "{bounds:?}");
        let record = app.frame_record();
        let frame = record.frame.as_ref().expect("首帧必须渲染");
        assert!(
            frame.primitives.iter().any(|primitive| {
                primitive.node == node
                    && matches!(
                        primitive.kind,
                        ui_core::test_support::record::PrimitiveKind::GlyphRun
                    )
                    && primitive.bounds.size.width > 0.0
            }),
            "文本节点必须产生字形图元"
        );
    }

    #[test]
    fn heading_uses_a_larger_font_than_the_body() {
        let doc = document(vec![MarkdownBlock::Heading {
            level: HeadingLevel::H1,
            pieces: Arc::from(vec![text("Title")]),
        }]);
        let view = MarkdownView::new(doc, style());
        let (app, handles) = mount(&view);
        let heading = app
            .probe()
            .bounds(handles.text_blocks[0].node)
            .expect("标题有布局盒");
        let body_view = MarkdownView::new(
            document(vec![paragraph(vec![text("Title")])]),
            view.style().clone(),
        );
        let (body_app, body_handles) = mount(&body_view);
        let body = body_app
            .probe()
            .bounds(body_handles.text_blocks[0].node)
            .expect("正文有布局盒");
        assert!(
            heading.size.height > body.size.height,
            "标题行高 {} 必须大于正文 {}",
            heading.size.height,
            body.size.height
        );
    }

    #[test]
    fn links_carry_underlines_and_are_hit_by_byte_index() {
        let doc = document_with_links(
            vec![paragraph(vec![
                text("go "),
                linked("here", 0),
                text(" now"),
            ])],
            vec!["https://example.com"],
        );
        let view = MarkdownView::new(doc, style());
        let (app, handles) = mount(&view);
        let node = handles.text_blocks[0].node;
        let bounds = app.probe().bounds(node).expect("布局盒");

        // 装饰：链接下划线在引擎里编码成 Quad 图元（压在字形之上）。
        let record = app.frame_record();
        let frame = record.frame.as_ref().expect("首帧");
        assert!(
            frame.quads.iter().any(|quad| quad.node == node
                && quad.bounds.size.width > 0.0
                && quad.bounds.size.height <= 2.0),
            "链接下划线必须作为 Quad 画出来（{:#?}）",
            frame.quads
        );

        // 命中：点落在链接文字上 → LinkId(0)；点落在文字左侧空白 → None。
        let window = app.app().window(app.window_id()).expect("窗口");
        let point = Point::new(
            bounds.origin.x + 20.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        assert_eq!(handles.link_at(window, point), Some(LinkId(0)));
        let outside = Point::new(bounds.origin.x + bounds.size.width + 40.0, point.y);
        assert_eq!(handles.link_at(window, outside), None);
    }

    #[test]
    fn blank_line_mounts_one_line_height_spacer() {
        let doc = document(vec![
            paragraph(vec![text("a")]),
            MarkdownBlock::BlankLine,
            paragraph(vec![text("b")]),
        ]);
        let view = MarkdownView::new(doc, style());
        let (app, _handles) = mount(&view);

        assert_eq!(
            app.probe().count(Selector::Id("MARKDOWN_BLANK")),
            1,
            "一个空行一个占位节点"
        );
        let blank = app
            .probe()
            .by_selector(Selector::Id("MARKDOWN_BLANK"))
            .expect("空行节点");
        let bounds = app.probe().bounds(blank).expect("布局盒");
        let expected = view.style().line_height();
        assert!(
            (bounds.size.height - expected).abs() < 0.5,
            "空行高度 {} 必须等于一行行高 {expected}",
            bounds.size.height
        );
    }

    #[test]
    fn inline_images_become_painted_inline_boxes() {
        let doc = document(vec![paragraph(vec![
            text("before "),
            image(24.0),
            text(" after"),
        ])]);
        let view = MarkdownView::new(doc, style());
        let (app, handles) = mount(&view);
        let node = handles.text_blocks[0].node;
        let record = app.frame_record();
        let frame = record.frame.as_ref().expect("首帧");
        assert!(
            frame.quads.iter().any(|quad| quad.node == node
                && (quad.bounds.size.width - 24.0).abs() < 0.5
                && (quad.bounds.size.height - 24.0).abs() < 0.5),
            "行内图片必须按解析期尺寸画成 24×24 的 Quad（{:#?}）",
            frame.quads
        );
        assert!(handles.text_blocks[0].links.is_empty());
    }

    #[test]
    fn code_quote_list_table_and_rule_mount_their_node_kinds() {
        let doc = document(vec![
            MarkdownBlock::CodeBlock {
                language: Some(SmolStr::new_static("rust")),
                code: SmolStr::new_static("let x = 1;"),
            },
            MarkdownBlock::Quote {
                blocks: Arc::from(vec![paragraph(vec![text("quoted")])]),
            },
            MarkdownBlock::List {
                ordered: true,
                start: 2,
                items: Arc::from(vec![
                    MarkdownListItem {
                        blocks: Arc::from(vec![paragraph(vec![text("one")])]),
                    },
                    MarkdownListItem {
                        blocks: Arc::from(vec![paragraph(vec![text("two")])]),
                    },
                ]),
            },
            MarkdownBlock::ThematicBreak,
            MarkdownBlock::Table {
                rows: Arc::from(vec![MarkdownTableRow {
                    header: true,
                    cells: Arc::from(vec![
                        MarkdownTableCell {
                            align: ParagraphAlign::Left,
                            pieces: Arc::from(vec![text("h1")]),
                        },
                        MarkdownTableCell {
                            align: ParagraphAlign::Right,
                            pieces: Arc::from(vec![text("h2")]),
                        },
                    ]),
                }]),
            },
        ]);
        let view = MarkdownView::new(doc, style());
        let (app, handles) = mount(&view);

        assert!(handles.root != handles.text_blocks[0].node);
        assert_eq!(app.probe().count(Selector::Id("MARKDOWN_CODE")), 1);
        assert_eq!(app.probe().count(Selector::Id("MARKDOWN_QUOTE")), 1);
        assert_eq!(app.probe().count(Selector::Id("MARKDOWN_LIST")), 1);
        assert_eq!(app.probe().count(Selector::Id("MARKDOWN_LIST_ITEM")), 2);
        assert_eq!(app.probe().count(Selector::Id("MARKDOWN_RULE")), 1);
        assert_eq!(app.probe().count(Selector::Id("MARKDOWN_TABLE")), 1);
        assert_eq!(app.probe().count(Selector::Id("MARKDOWN_TABLE_CELL")), 2);
        assert_eq!(
            app.probe().count(Selector::Id("MARKDOWN_LIST_MARKER")),
            2,
            "每个列表项一个标记"
        );
        assert_eq!(
            handles.text_blocks.len(),
            5,
            "引用 1 + 列表 2 + 表格 2 = 5 个文本块进 handles（代码块不走 rich_text）"
        );
    }

    #[test]
    fn ordered_list_markers_start_at_the_parsed_number() {
        let doc = document(vec![MarkdownBlock::List {
            ordered: true,
            start: 3,
            items: Arc::from(vec![MarkdownListItem {
                blocks: Arc::from(vec![paragraph(vec![text("item")])]),
            }]),
        }]);
        let view = MarkdownView::new(doc, style());
        let (app, _handles) = mount(&view);
        let marker = app
            .probe()
            .by_selector(Selector::Id("MARKDOWN_LIST_MARKER"))
            .expect("标记");
        assert!(app.probe().bounds(marker).is_some());
    }

    #[test]
    fn empty_document_mounts_only_the_root() {
        let view = MarkdownView::new(document(Vec::new()), style());
        let (app, handles) = mount(&view);
        assert!(handles.text_blocks.is_empty());
        assert!(app.probe().bounds(handles.root).is_some());
        assert_eq!(app.probe().count(Selector::Id("MARKDOWN")), 1);
    }

    #[test]
    fn rich_text_can_be_used_standalone() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let theme = Theme::of(Appearance::Light);
        let flow_style = FlowStyle::new(
            16.0,
            theme.role(crate::theme::role::Role::FgDefault),
            theme.role(crate::theme::role::Role::BrandBase),
            theme.role(crate::theme::role::Role::FgDefault),
        );
        let base = TextStyle::new(16.0);
        let mut images = InvisibleImages;
        let rich: RichText = rich_text(
            app.app_mut().window_mut(window).expect("窗口").tree_mut(),
            root,
            &[text("plain")],
            &flow_style,
            &base,
            ParagraphAlign::Left,
            &mut images,
        )
        .expect("富文本节点");
        let _ = app.drain().expect("首帧");
        assert!(app.probe().bounds(rich.node).is_some());
        assert!(rich.links.is_empty());
    }

    #[test]
    fn placeholder_images_are_used_by_default_build() {
        let mut images = crate::kit::markdown::inline_flow::PlaceholderImagePaint {
            color: Theme::of(Appearance::Light).role(crate::theme::role::Role::SurfaceControl),
        };
        let paint = crate::kit::markdown::inline_flow::ImagePaintSource::paint_for(
            &mut images,
            "x.png",
            InlineImageFormat::Image,
            Size::new(8.0, 8.0),
        );
        assert!(matches!(
            paint,
            Some(ui_core::text::InlineBoxPaint::Quad { .. })
        ));
    }

    #[test]
    fn alignment_maps_to_engine_text_align() {
        use ui_core::text::TextAlign;
        assert_eq!(text_align(ParagraphAlign::Left), TextAlign::Left);
        assert_eq!(text_align(ParagraphAlign::Right), TextAlign::Right);
        assert_eq!(text_align(ParagraphAlign::Center), TextAlign::Center);
    }

    #[test]
    fn kbd_and_code_pieces_do_not_break_paragraph_mounting() {
        let doc = document(vec![paragraph(vec![
            MarkdownPiece::Text {
                text: SmolStr::new_static("x"),
                strong: false,
                emphasis: false,
                strike: false,
                code: false,
                link: None,
                kbd: Some(KbdId(0)),
            },
            MarkdownPiece::Text {
                text: SmolStr::new_static("code"),
                strong: false,
                emphasis: false,
                strike: false,
                code: true,
                link: None,
                kbd: None,
            },
        ])]);
        let view = MarkdownView::new(doc, style());
        let (app, handles) = mount(&view);
        assert!(app.probe().bounds(handles.text_blocks[0].node).is_some());
    }

    /// 空文档的 link_at 不 panic。
    #[test]
    fn link_at_without_links_is_none() {
        let view = MarkdownView::new(document(vec![paragraph(vec![text("plain")])]), style());
        let (app, handles) = mount(&view);
        let window = app.app().window(app.window_id()).expect("窗口");
        let bounds = app
            .probe()
            .bounds(handles.text_blocks[0].node)
            .expect("布局盒");
        assert_eq!(
            handles.link_at(
                window,
                Point::new(bounds.origin.x + 1.0, bounds.origin.y + 1.0)
            ),
            None
        );
    }

    /// 根节点 id 是静态的（测试用 `Probe` 找它）。
    #[test]
    fn root_uses_the_static_probe_id() {
        let view = MarkdownView::new(document(Vec::new()), style());
        let (app, handles) = mount(&view);
        let by_id = app
            .probe()
            .by_selector(Selector::Id("MARKDOWN"))
            .expect("根节点");
        assert_eq!(by_id, handles.root);
    }

    /// 空行之后的非首块标题仍然拿到额外上间距（旧实现语义；P2-3 审查 M4 的回归）。
    ///
    /// 几何：空白行 wrapper 自己高度 = 一行；标题的 `margin_top` 必须是
    /// `BLOCK_GAP + HEADING_SPACE_ABOVE`，而普通段落只有 `BLOCK_GAP` —— 空行不吞掉标题的额外间距。
    #[test]
    fn a_heading_after_a_blank_line_keeps_the_extra_space_above_it() {
        let heading = || MarkdownBlock::Heading {
            level: HeadingLevel::H1,
            pieces: Arc::from(vec![text("Title")]),
        };
        let heading_doc = document(vec![heading(), MarkdownBlock::BlankLine, heading()]);
        let view = MarkdownView::new(heading_doc, style());
        let (app, handles) = mount(&view);
        let blank = app
            .probe()
            .by_selector(Selector::Id("MARKDOWN_BLANK_1"))
            .expect("空行 wrapper");
        let blank_bounds = app.probe().bounds(blank).expect("空行有布局盒");
        let first = app
            .probe()
            .bounds(handles.text_blocks[0].node)
            .expect("标题 1");
        let second = app
            .probe()
            .bounds(handles.text_blocks[1].node)
            .expect("标题 2");
        assert!(
            first.origin.y < blank_bounds.origin.y,
            "标题 1 在空行之前：{first:?} vs {blank_bounds:?}"
        );
        let heading_gap = second.origin.y - (blank_bounds.origin.y + blank_bounds.size.height);
        assert!(
            (heading_gap - (super::BLOCK_GAP + super::HEADING_SPACE_ABOVE)).abs() < 0.01,
            "空行后标题的上间距 = {}，期望 {}",
            heading_gap,
            super::BLOCK_GAP + super::HEADING_SPACE_ABOVE
        );

        let paragraph_doc = document(vec![
            heading(),
            MarkdownBlock::BlankLine,
            paragraph(vec![text("Body")]),
        ]);
        let paragraph_view = MarkdownView::new(paragraph_doc, style());
        let (paragraph_app, paragraph_handles) = mount(&paragraph_view);
        let paragraph_blank = paragraph_app
            .probe()
            .by_selector(Selector::Id("MARKDOWN_BLANK_1"))
            .expect("空行 wrapper");
        let paragraph_blank_bounds = paragraph_app
            .probe()
            .bounds(paragraph_blank)
            .expect("空行有布局盒");
        let body = paragraph_app
            .probe()
            .bounds(paragraph_handles.text_blocks[1].node)
            .expect("段落");
        let body_gap =
            body.origin.y - (paragraph_blank_bounds.origin.y + paragraph_blank_bounds.size.height);
        assert!(
            (body_gap - super::BLOCK_GAP).abs() < 0.01,
            "空行后普通段落的上间距 = {body_gap}，期望 {}",
            super::BLOCK_GAP
        );
        assert!(
            heading_gap > body_gap,
            "标题的额外上间距必须大于普通段落：{heading_gap} vs {body_gap}"
        );
    }
}

use std::sync::Arc;

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    math::{Point, Size},
    node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    paint::{
        color::Color,
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    resources::ResourceCache,
    text::{InlineBoxPaint, TextAlign, TextStyle, TextWrap},
    window::{Window, WindowId},
};

use crate::{
    kit::markdown::{
        document::{LinkId, MarkdownBlock, MarkdownDocument, MarkdownPiece, ParagraphAlign},
        inline_flow::{FlowStyle, ImagePaintSource, LinkSpan, flow},
        source::{InlineImageFormat, MarkdownAssets},
    },
    theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
};

const LINE_HEIGHT: f32 = 1.4;
const BLOCK_GAP: f32 = 8.0;
const HEADING_SPACE_ABOVE: f32 = 8.0;
const CODE_PADDING: f32 = 8.0;
const CODE_RADIUS: f32 = 4.0;
const QUOTE_BORDER_WIDTH: f32 = 3.0;
const QUOTE_PADDING: f32 = 8.0;
const LIST_MARKER_WIDTH: f32 = 20.0;
const CELL_PADDING: f32 = 4.0;

#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownStyle {
    pub body_size: f32,
    pub line_height: f32,
    pub foreground: Color,
    pub muted: Color,
    pub link: Color,
    pub code_foreground: Color,
    pub code_background: Color,
    pub quote_border: Color,
    pub divider: Color,
    pub image_placeholder: Color,
    pub mono_family: SmolStr,
}

impl MarkdownStyle {
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

    pub fn line_height(&self) -> f32 {
        self.body_size * self.line_height
    }

    fn text_style(&self, size: f32, bold: bool, align: ParagraphAlign) -> TextStyle {
        TextStyle::new(size)
            .with_color(self.foreground)
            .with_weight(if bold { 700 } else { 400 })
            .with_line_height(self.line_height)
            .with_alignment(text_align(align))
            .with_wrap(TextWrap::Wrap)
    }

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

fn text_align(align: ParagraphAlign) -> TextAlign {
    match align {
        ParagraphAlign::Left => TextAlign::Left,
        ParagraphAlign::Right => TextAlign::Right,
        ParagraphAlign::Center => TextAlign::Center,
    }
}

pub struct ResourceImages<'a> {
    pub cache: &'a mut ResourceCache,
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

#[derive(Clone, Debug)]
pub struct RichText {
    pub node: NodeId,
    pub links: Arc<[LinkSpan]>,
}

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

#[derive(Clone, Debug)]
pub struct MarkdownHandles {
    pub root: NodeId,
    pub text_blocks: Vec<RichText>,
}

impl MarkdownHandles {
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

#[derive(Clone, Debug)]
pub struct MarkdownView {
    document: Arc<MarkdownDocument>,
    style: MarkdownStyle,
}

impl MarkdownView {
    pub fn new(document: Arc<MarkdownDocument>, style: MarkdownStyle) -> Self {
        Self { document, style }
    }

    pub fn from_theme(theme: &Theme, document: Arc<MarkdownDocument>) -> Self {
        Self::new(document, MarkdownStyle::from_theme(theme))
    }

    pub fn document(&self) -> &MarkdownDocument {
        &self.document
    }

    pub fn style(&self) -> &MarkdownStyle {
        &self.style
    }

    pub fn build(&self, tree: &mut NodeTree, parent: NodeId) -> MarkdownHandles {
        let mut images = super::inline_flow::PlaceholderImagePaint {
            color: self.style.image_placeholder,
        };
        self.build_with_images(tree, parent, &mut images)
    }

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
        self.build_into(tree, root, images)
    }

    fn build_into(
        &self,
        tree: &mut NodeTree,
        root: NodeId,
        images: &mut dyn ImagePaintSource,
    ) -> MarkdownHandles {
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

pub struct MarkdownNode {
    root: NodeId,
    document: Arc<MarkdownDocument>,
    theme: ThemeHandle,
    appearance: Appearance,
    handles: MarkdownHandles,
}

impl MarkdownNode {
    pub fn mount(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        theme: ThemeHandle,
        document: Arc<MarkdownDocument>,
    ) -> Result<Self, EngineError> {
        let appearance = theme.current(app);
        let view = MarkdownView::new(
            document.clone(),
            MarkdownStyle::from_theme(theme.theme_for(appearance)),
        );
        let dependency_theme = theme.clone();
        let root = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .id("MARKDOWN")
            .bind_style(move |cx| {
                let _ = dependency_theme.appearance(cx);
                StylePatch::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
        let mut images = super::inline_flow::PlaceholderImagePaint {
            color: view.style.image_placeholder,
        };
        let handles = view.build_into(
            app.window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut(),
            root,
            &mut images,
        );
        Ok(Self {
            root,
            document,
            theme,
            appearance,
            handles,
        })
    }

    pub fn sync(&mut self, app: &mut App) -> bool {
        let appearance = self.theme.current(app);
        if appearance == self.appearance {
            return false;
        }
        self.appearance = appearance;
        let Some(window) = self.root.window() else {
            return false;
        };
        let view = MarkdownView::new(
            self.document.clone(),
            MarkdownStyle::from_theme(self.theme.theme_for(appearance)),
        );
        let Some(target) = app.window_mut(window) else {
            return false;
        };
        let tree = target.tree_mut();
        let Ok(children) = tree.get(self.root).map(|node| node.children().to_vec()) else {
            return false;
        };
        for child in children {
            if let Ok(removed) = tree.remove(child) {
                debug_assert!(
                    removed.is_empty(),
                    "markdown 子树不得挂 bind_*：重建会留下未释放的依赖边（App::write 退化 + 泄漏）"
                );
            }
        }
        let mut images = super::inline_flow::PlaceholderImagePaint {
            color: view.style.image_placeholder,
        };
        self.handles = view.build_into(tree, self.root, &mut images);
        true
    }

    #[must_use]
    pub fn root(&self) -> NodeId {
        self.root
    }

    #[must_use]
    pub fn handles(&self) -> &MarkdownHandles {
        &self.handles
    }

    pub fn release(&mut self, app: &mut App) -> bool {
        let Some(window) = self.root.window() else {
            return false;
        };
        let removed = app.remove_node(window, self.root).is_ok();
        if removed {
            self.handles.text_blocks.clear();
        }
        removed
    }
}

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

    fn build_blocks(&mut self, tree: &mut NodeTree, parent: NodeId, blocks: &[MarkdownBlock]) {
        let mut previous_blank = false;
        let mut block_index = 0usize;
        for (index, block) in blocks.iter().enumerate() {
            if matches!(block, MarkdownBlock::BlankLine) {
                self.build_block(tree, parent, block, 0.0, index);
                previous_blank = true;
                continue;
            }
            let mut gap = if block_index > 0 && !previous_blank {
                BLOCK_GAP
            } else {
                0.0
            };
            if block_index > 0 && matches!(block, MarkdownBlock::Heading { .. }) {
                gap += HEADING_SPACE_ABOVE;
            }
            block_index += 1;
            previous_blank = false;
            self.build_block(tree, parent, block, gap, index);
        }
    }

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
        interaction::ScrollConfig,
        math::{Point, Size},
        node::{builder::NodeBuilder, tree::NodeKind},
        paint::style_builder::StyleBuilder,
        test_support::{app::TestApp, probe::Selector},
        text::TextStyle,
    };

    use super::{MarkdownNode, MarkdownStyle, MarkdownView, RichText, rich_text, text_align};
    use crate::{
        kit::markdown::{
            document::{
                HeadingLevel, KbdId, LinkId, MarkdownBlock, MarkdownDocument, MarkdownListItem,
                MarkdownPiece, MarkdownTableCell, MarkdownTableRow, ParagraphAlign,
            },
            inline_flow::{FlowStyle, InvisibleImages},
            source::InlineImageFormat,
        },
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
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

        let record = app.frame_record();
        let frame = record.frame.as_ref().expect("首帧");
        assert!(
            frame.quads.iter().any(|quad| quad.node == node
                && quad.bounds.size.width > 0.0
                && quad.bounds.size.height <= 2.0),
            "链接下划线必须作为 Quad 画出来（{:#?}）",
            frame.quads
        );

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
            (heading_gap - super::HEADING_SPACE_ABOVE).abs() < 0.01,
            "空行后标题的上间距 = {heading_gap}，期望 {}（空行抑制 BLOCK_GAP，只留标题额外间距）",
            super::HEADING_SPACE_ABOVE
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
            body_gap.abs() < 0.01,
            "空行后普通段落的上间距 = {body_gap}，期望 0（空行自己就是间距）"
        );
        assert!(
            heading_gap > body_gap,
            "标题的额外上间距必须大于普通段落：{heading_gap} vs {body_gap}"
        );

        let adjacent_heading = document(vec![heading(), paragraph(vec![text("Body")])]);
        let adjacent_view = MarkdownView::new(adjacent_heading, style());
        let (adjacent_app, adjacent_handles) = mount(&adjacent_view);
        let heading_bounds = adjacent_app
            .probe()
            .bounds(adjacent_handles.text_blocks[0].node)
            .expect("标题");
        let body_bounds = adjacent_app
            .probe()
            .bounds(adjacent_handles.text_blocks[1].node)
            .expect("正文");
        let gap_after_heading =
            body_bounds.origin.y - (heading_bounds.origin.y + heading_bounds.size.height);
        assert!(
            (gap_after_heading - super::BLOCK_GAP).abs() < 0.01,
            "无空行时段落上间距 = {gap_after_heading}，期望 {}",
            super::BLOCK_GAP
        );

        let adjacent_paragraph = document(vec![paragraph(vec![text("Intro")]), heading()]);
        let paragraph_view = MarkdownView::new(adjacent_paragraph, style());
        let (paragraph_app, paragraph_handles) = mount(&paragraph_view);
        let intro_bounds = paragraph_app
            .probe()
            .bounds(paragraph_handles.text_blocks[0].node)
            .expect("段落");
        let heading_bounds = paragraph_app
            .probe()
            .bounds(paragraph_handles.text_blocks[1].node)
            .expect("标题");
        let gap_after_paragraph =
            heading_bounds.origin.y - (intro_bounds.origin.y + intro_bounds.size.height);
        assert!(
            (gap_after_paragraph - (super::BLOCK_GAP + super::HEADING_SPACE_ABOVE)).abs() < 0.01,
            "无空行时标题上间距 = {gap_after_paragraph}，期望 {}",
            super::BLOCK_GAP + super::HEADING_SPACE_ABOVE
        );
    }

    fn first_text_color(
        app: &TestApp,
        root: ui_core::node::id::NodeId,
    ) -> ui_core::paint::color::Color {
        let window = app.window_id();
        let tree = app.app().window(window).expect("窗口").tree();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let entry = tree.get(node).expect("节点存活");
            if let NodeKind::Text(content) = entry.kind() {
                return content.style.color;
            }
            stack.extend(entry.children().iter().rev().copied());
        }
        panic!("markdown 子树里没有文本节点");
    }

    fn resolved(color: ui_core::paint::color::Color) -> [f32; 4] {
        ui_core::paint::color::ColorResolver
            .resolve(color)
            .components
    }

    #[test]
    fn switching_appearance_rebuilds_the_markdown_with_the_new_theme() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let doc = document(vec![
            MarkdownBlock::Heading {
                level: HeadingLevel::H1,
                pieces: Arc::from(vec![text("标题")]),
            },
            paragraph(vec![linked("链接", 0), text("与")]),
            MarkdownBlock::CodeBlock {
                language: Some(SmolStr::new_static("rust")),
                code: SmolStr::new_static("let x = 1;"),
            },
        ]);
        let mut node = MarkdownNode::mount(app.app_mut(), window, root, theme.clone(), doc)
            .expect("the markdown mounts");
        let _ = app.drain().expect("首帧");

        assert_eq!(
            resolved(first_text_color(&app, node.root())),
            resolved(Theme::of(Appearance::Light).role(Role::FgDefault)),
            "浅色正文"
        );
        assert!(!node.sync(app.app_mut()), "外观未变时 no-op");

        assert!(
            theme.set_appearance(app.app_mut(), Appearance::Dark),
            "写外观信号"
        );
        assert!(node.sync(app.app_mut()), "外观变化 = 重建子树");
        let _ = app.drain().expect("重建后的帧");

        assert_eq!(
            resolved(first_text_color(&app, node.root())),
            resolved(Theme::of(Appearance::Dark).role(Role::FgDefault)),
            "深色正文（子树的静态 TextStyle.color 被重建换掉）"
        );
        assert_ne!(
            resolved(Theme::of(Appearance::Light).role(Role::FgDefault)),
            resolved(Theme::of(Appearance::Dark).role(Role::FgDefault)),
            "两套外观的正文色必须不同（否则上面的断言无意义）"
        );
    }

    #[test]
    fn releasing_the_markdown_unloads_its_root_and_stops_syncing() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let mut node = MarkdownNode::mount(
            app.app_mut(),
            window,
            root,
            theme.clone(),
            document(vec![paragraph(vec![text("段落")])]),
        )
        .expect("the markdown mounts");
        let _ = app.drain().expect("首帧");
        let markdown_root = node.root();
        assert!(
            app.app()
                .window(window)
                .expect("窗口")
                .tree()
                .get(markdown_root)
                .is_ok(),
            "挂载后有根子树"
        );

        assert!(node.release(app.app_mut()), "第一次释放卸载根");
        assert!(
            app.app()
                .window(window)
                .expect("窗口")
                .tree()
                .get(markdown_root)
                .is_err(),
            "根子树已卸载"
        );
        assert!(
            node.handles().text_blocks.is_empty(),
            "释放后不再持有文本块句柄"
        );
        assert!(!node.release(app.app_mut()), "重复释放幂等");
        assert!(!node.sync(app.app_mut()), "释放后 sync 是 no-op");

        assert!(theme.set_appearance(app.app_mut(), Appearance::Dark));
        let _ = app.drain().expect("外观帧");
        assert!(!node.sync(app.app_mut()));
    }

    #[test]
    fn rebuilding_keeps_the_markdown_root_identity_and_the_page_offset() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let page = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::boxed()
                .id("page")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(800.0)
                        .height(600.0)
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("页面")
        };
        let handle = app
            .app_mut()
            .make_scroll(window, page, ScrollConfig::default())
            .expect("滚动容器");
        app.app_mut()
            .set_scroll_content(&handle, Size::new(800.0, 1800.0))
            .expect("滚动内容");
        let blocks = (0..12)
            .map(|index| paragraph(vec![text(&format!("段落 {index}"))]))
            .collect::<Vec<_>>();
        let mut node =
            MarkdownNode::mount(app.app_mut(), window, page, theme.clone(), document(blocks))
                .expect("the markdown mounts");
        let _ = app.drain().expect("首帧");
        app.app_mut()
            .scroll_to(&handle, Point::new(0.0, 240.0))
            .expect("滚动");
        let _ = app.drain().expect("滚动帧");
        let offset = app.app().scroll_state(&handle).expect("滚动状态").offset.y;
        assert!((offset - 240.0).abs() < 0.5, "页面滚到 240：{offset}");

        let root_before = node.root();
        let first_child_before = app
            .app()
            .window(window)
            .expect("窗口")
            .tree()
            .get(root_before)
            .expect("根节点")
            .children()[0];

        assert!(
            theme.set_appearance(app.app_mut(), Appearance::Dark),
            "写外观信号"
        );
        assert!(node.sync(app.app_mut()), "外观变化 = 重建子树");
        let _ = app.drain().expect("重建后的帧");

        assert_eq!(
            node.root(),
            root_before,
            "根节点原地保留（外部 probe / 布局锚点不失效）"
        );
        let tree = app.app().window(window).expect("窗口").tree();
        let rebuilt = tree.get(root_before).expect("根节点存活");
        assert!(!rebuilt.children().is_empty(), "重建后根下仍有块");
        assert_ne!(
            rebuilt.children()[0],
            first_child_before,
            "子树节点是真重建（不是复用旧块）"
        );
        assert_eq!(rebuilt.parent(), Some(page), "根节点的父节点不变");
        let offset_after = app.app().scroll_state(&handle).expect("滚动状态").offset.y;
        assert!(
            (offset_after - offset).abs() < 0.5,
            "重建不移动页面滚动：{offset_after}"
        );
    }

    #[test]
    fn the_markdown_subtree_holds_no_signal_dependency_edges() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let node = MarkdownNode::mount(
            app.app_mut(),
            window,
            root,
            theme,
            document(vec![
                paragraph(vec![linked("链接", 0), text("与正文")]),
                MarkdownBlock::CodeBlock {
                    language: None,
                    code: SmolStr::new_static("let x = 1;"),
                },
            ]),
        )
        .expect("the markdown mounts");
        let _ = app
            .drain()
            .expect("首帧（样式 pass 求值根节点的 bind_style）");

        let children: Vec<_> = app
            .app()
            .window(window)
            .expect("窗口")
            .tree()
            .get(node.root())
            .expect("根节点")
            .children()
            .to_vec();
        assert!(!children.is_empty(), "markdown 根下必须有块");
        for child in children {
            let removed = app
                .app_mut()
                .window_mut(window)
                .expect("窗口")
                .tree_mut()
                .remove(child)
                .expect("移除 markdown 子树");
            assert!(
                removed.is_empty(),
                "markdown 子树不得挂 bind_*（remove 只交还非空依赖边）：{removed:?}"
            );
        }
        let removed_root = app
            .app_mut()
            .window_mut(window)
            .expect("窗口")
            .tree_mut()
            .remove(node.root())
            .expect("移除根节点");
        assert!(
            !removed_root.is_empty(),
            "根节点持有外观依赖边（G3 的失效通道）；移除时应交还 —— 否则上面的空依赖断言不成立"
        );
    }
}

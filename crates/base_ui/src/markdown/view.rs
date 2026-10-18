//! markdown 组件的视图层：状态实体、元素包装与排版常量。
//!
//! 每帧只做"IR → 元素"的廉价构建：块内容（`Arc<[MarkdownPiece]>`）在解析时就已经算好，
//! 这里只搬 `Arc`、构造轻量元素；`parse::parse_document` 只在 [`MarkdownState::new`] /
//! [`MarkdownState::set_source`] 里各调用一次。

use gpui_kit::{
    App, Context, ElementId, Entity, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    Rems, Render, RenderOnce, SharedString, StyleRefinement, Styled, Window, base::StyledExt, div,
    rems,
};

use super::{
    document::{LinkId, MarkdownBlock, MarkdownDocument, ParagraphAlign},
    inline_flow::TextFlow,
    parse,
};
use crate::style::theme::ThemeExt;

/// 两个相邻块（源码里中间没有空行）之间的最小垂直间距。
///
/// 源码里写了空行时，那段间距由空行块自己按一行正文行高画出（见 [`MarkdownBlock::BlankLine`]），
/// 这段最小间距就不再叠加——`a\n\nb` 之间的空行正好是一行行高。
const BLOCK_GAP: Rems = rems(0.5);
/// 非首个标题与上一块之间的额外间距。
const HEADING_SPACE_ABOVE: Rems = rems(0.5);

/// 一次鼠标按下的记录：按在哪条链接上（可能不在链接上）与按在窗口的哪个位置。
///
/// 记位置是为了让"按下点与松开点必须落在同一个块的同一条链接上"可判定：
/// `LinkId` 是整篇文档级的，同一份 `MarkdownState` 被多处渲染时两个实例的链接会重名。
#[derive(Copy, Clone)]
pub(crate) struct PressedLink {
    pub link: Option<LinkId>,
    pub at: Point<Pixels>,
}

pub struct MarkdownState {
    source: SharedString,
    document: MarkdownDocument,
    hovered_link: Option<LinkId>,
    pressed: Option<PressedLink>,
}

impl MarkdownState {
    /// 解析一次 markdown，之后每帧只复用 `document`。
    ///
    /// `cx` 只用来读资源：没写 `size` 的图片按资源自身的尺寸绘制，资源取自 `cx` 的
    /// `AssetSource`（见 [`super::source`]）。
    pub fn new(source: impl Into<SharedString>, cx: &App) -> Self {
        let source = source.into();
        let document = parse::parse_document(source.as_ref(), cx.asset_source().as_ref());
        Self {
            source,
            document,
            hovered_link: None,
            pressed: None,
        }
    }

    /// 换掉整篇文档并重新解析；原文没变时什么都不做。
    ///
    /// 解析是这个组件最贵的一步，而"每帧按当前语言把消息解析成原文再交过来"的调用方
    /// （框架的占位就是）只有靠这里挡一下，才不会每帧重解析同一段文字。顺带也把 hover /
    /// 按下记录留住了：原文没变，旧文档里的链接仍指向同一批 `LinkId`，清掉它们只会让
    /// 鼠标停在链接上时下划线闪一下。
    pub fn set_source(&mut self, source: impl Into<SharedString>, cx: &mut Context<Self>) {
        let source = source.into();
        if self.source == source {
            return;
        }
        self.document = parse::parse_document(source.as_ref(), cx.asset_source().as_ref());
        self.source = source;
        self.hovered_link = None;
        // 新文档里的链路与旧文档无关：按下点连同它的位置一起作废。
        self.pressed = None;
        cx.notify();
    }

    /// 当前 markdown 原文。
    pub fn source(&self) -> &SharedString {
        &self.source
    }

    /// 这篇文档被"读"而不是被"看"时说的话，语言与它的原文被解析时一致。
    /// 见 [`MarkdownDocument::plain_text`]。
    ///
    /// IR 只在本模块内可见，所以需要给这篇文档起个名字的调用方（例如本组件画出的占位用它做
    /// 可访问名称）只能从这里读到它。
    pub fn plain_text(&self) -> SharedString {
        self.document.plain_text()
    }

    /// 最近一次 hover 信号指向的链接：它只承担"要不要重绘"与可观测状态的职责，
    /// 画什么由每个 `TextFlow` 实例按自己的命中盒与指针位置现算（见 `inline_flow::hovered_link`）。
    /// 只有一个 `Markdown` 实例时，它就等于当前 hover 的链接。
    pub(crate) fn hovered_link(&self) -> Option<LinkId> {
        self.hovered_link
    }

    pub(crate) fn set_hovered_link(&mut self, link: Option<LinkId>, cx: &mut Context<Self>) {
        if self.hovered_link == link {
            return;
        }
        self.hovered_link = link;
        cx.notify();
    }

    /// 最近一次鼠标按下的记录：按在哪条链接上（`None` 表示按在空白上）与按在哪里。
    ///
    /// 记下位置，是为了让"这次按下是不是发生在本块里"可以被本块的几何判定：
    /// `LinkId` 是整篇文档级的，同一份 `MarkdownState` 被多处渲染时两个实例画的是同一条链接、
    /// 取到的是同一个 id，只比较 id 会把"在副本 1 上按下、在副本 2 上抬起"当成一次点击。
    pub(crate) fn pressed(&self) -> Option<PressedLink> {
        self.pressed
    }

    /// 记录或清空按下状态。它不影响任何绘制，所以不 `cx.notify()`。
    pub(crate) fn set_pressed(&mut self, pressed: Option<PressedLink>) {
        self.pressed = pressed;
    }
}

/// markdown 组件的元素包装，形态与 [`Input`](crate::input::Input) 一致：
/// 状态由 [`MarkdownState`] 持有，这里只负责样式细化。
#[derive(IntoElement)]
pub struct Markdown {
    state: Entity<MarkdownState>,
    style: StyleRefinement,
}

impl Markdown {
    pub fn new(state: &Entity<MarkdownState>) -> Self {
        Self {
            state: state.clone(),
            style: StyleRefinement::default(),
        }
    }
}

impl Styled for Markdown {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Markdown {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div().child(self.state).refine_style(&self.style)
    }
}

impl Render for MarkdownState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.weak_entity();
        let link_color = cx.theme().roles.brand.base.hsla;
        // 一行正文行高：空行不属于任何块，所以不跟着标题放大，任何文档里都一样高。
        let line_height = window.text_style().line_height_in_pixels(window.rem_size());
        let mut column = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .debug_selector(|| "BASE_UI_MARKDOWN".to_string());
        // 上一块是不是空行：空行自己就是间距，它两侧不再另加块间距。
        let mut previous_blank = false;
        // 调试选择器里的编号按各自的类别数：`BASE_UI_MARKDOWN_BLOCK_{n}` 数的是画出来的块
        // （标题与段落），`BASE_UI_MARKDOWN_BLANK_{n}` 数的是空行。空行不该让"第几个块"跟着漂。
        let mut block_index = 0;
        let mut blank_index = 0;

        for block in self.document.blocks.iter() {
            let (pieces, align, scale, force_bold) = match block {
                // 空行不属于任何块：它自己就是一行正文行高，两侧的块间距因此不再叠加。
                MarkdownBlock::BlankLine => {
                    let index = blank_index;
                    blank_index += 1;
                    column = column.child(
                        div()
                            .h(line_height)
                            .debug_selector(move || format!("BASE_UI_MARKDOWN_BLANK_{index}")),
                    );
                    previous_blank = true;
                    continue;
                }
                MarkdownBlock::Heading { level, pieces } => {
                    (pieces.clone(), ParagraphAlign::Left, level.scale(), true)
                }
                MarkdownBlock::Paragraph { align, pieces } => (pieces.clone(), *align, 1.0, false),
            };
            let index = block_index;
            block_index += 1;

            // 每个块有稳定且兄弟间唯一的 `ElementId`：鼠标回调与命中盒的归属靠它区分。
            //
            // 只保证**兄弟之间**唯一，所以不要给 `TextFlow` 引入
            // `Window::with_optional_element_state` 之类的按 id 存取的元素状态：同一份
            // `MarkdownState` 被两处渲染时，两个实例的块会取到同一个 id，元素状态会互相串位
            let flow = TextFlow::new(
                ElementId::Integer(index as u64),
                pieces,
                scale,
                force_bold,
                align,
                self.document.links.clone(),
                entity.clone(),
                link_color,
                window,
            );

            let mut cell = div()
                .min_w_0()
                .debug_selector(move || format!("BASE_UI_MARKDOWN_BLOCK_{index}"));
            // 与上一块之间的间距：源码里没有空行的相邻两块留最小间距 `BLOCK_GAP`（空行两边不加，
            // 那段间距由空行自己占）；非首块的标题在这之上再拉一段 `HEADING_SPACE_ABOVE`。首块
            // 不留上间距，避免文档顶部多出一段空白。
            let mut gap = if index > 0 && !previous_blank {
                BLOCK_GAP
            } else {
                Rems::ZERO
            };
            if index > 0 && matches!(block, MarkdownBlock::Heading { .. }) {
                gap += HEADING_SPACE_ABOVE;
            }
            cell = cell.mt(gap);

            previous_blank = false;
            column = column.child(cell.child(flow));
        }

        column
    }
}

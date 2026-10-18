//! CommonMark 源码 → [`MarkdownDocument`]。
//!
//! 解析用 comrak 的默认选项（纯 CommonMark、无扩展），结果在这里一次性转成自有 IR：
//! comrak 的 AST 借用函数内的局部 arena，出了这个函数就不复存在。
//!
//! 只处理 `Document` / `Paragraph` / `Heading` / `Strong` / `Emph` / `Link` / `Image` /
//! `Text` / `SoftBreak` / `LineBreak`，外加 HTML 块里的 `<p align="…">…</p>` 标签对
//! （它按跨过的内容产出块：段落带这一对的对齐、标题照旧是标题，见 [`pair_blocks`]）；
//! **其余一切节点连同整棵子树丢弃**（不递归）。因此围栏代码块、`- item`、`> quote`、
//! 不是 `file:` URI 的图片连 alt 文本在内**一个字都不会显示**；HTML 块里除了那种标签对，
//! 别的标签与文字照旧不显示（`<div>…</div>` 里的普通文字不显示，它里面写了对齐标签对时
//! 那一对会显示）。
//!
//! 图片只认 `![alt](file://{asset_path}?…)` 这一种写法（语法见 [`super::source`]）；
//! alt 文本照旧不显示，`![alt](https://…)` 之类的图片整张忽略。
//!
//! **空行**：两个块之间的每一个空行都产出 [`MarkdownBlock::BlankLine`]，排版层把每一个画成一行
//! 正文行高——`a\n\nb` 是"一段 + 一行空行 + 一段"，`a\n\n\n\nb` 是三行空行，连续几个空行就画
//! 几行（不折叠）。空行按**原文里的行**数，不按两个块的行号差算：中间夹着被丢弃的块级结构时
//! （`a\n\n- item\n\nb`），只有那两行真的空行算数，`- item` 占的那一行不算。
//!
//! 空行只画在**两个真的产出块的块级结构之间**：文档开头、结尾，以及第一个产出块之前（前面只有
//! 被丢弃的块、或者只有空标签对）的空行都不画——`"\n\na"`、`"a\n\n"`、`"<p align=\"center\"></p>\n\na"`
//! 都只画 `a` 那一个块。
//!
//! 对齐标签对的适用范围（不生效时**不报错**：标签对没配成、或者落在不支持的块里，那一块就按
//! 上面的规则丢弃；把标签名写错成 `<palign="center">` 时那一行根本不是 HTML 块，会按普通段落
//! 显示出来）：
//!
//! - 标签对必须在**顶层 HTML 块**里：开标签可以是这一块里任意位置上的 `<p …>` 元素，行首
//!   （最多三个前导空格，与其它块一样）与跟在块内别的标签后面（`<div>x <p align="center">y</p>`）
//!   都算。写进列表项、引用或四空格缩进的代码块里不生效（那些节点整棵丢弃），写在段落中间也不
//!   生效（那一行是段落不是 HTML 块，行内 HTML 只认 `<kbd>`）。
//! - 闭标签是**第一个不在注释里的** `</p>`（见 [`find_close_tag`]），**中间可以有空行**：标签对
//!   因此能跨空行闭合（`<p align="center">a\n\nb</p>` 是"居中一段 + 一行空行 + 居中一段"）。
//! - 配对成功的标签对把闭标签之前的原文整段当自己的内容（见 [`pair_blocks`]）：内容照常产出块
//!   ——段落带上标签对的对齐，标题照旧是标题，空行照旧是空行；其余块级结构（列表、引用、代码块、
//!   `<div>` 之类的 HTML 块…）连同子树丢弃，它们在组件的任何位置都不画。落在内容里的顶层节点
//!   不再各自成块（见 [`parse_document`] 里的 `consumed_before`），它们的内容由这一对产出。
//! - 跨空行往后找闭标签时，**下一个 HTML 块里的候选开标签就是终点**（见 [`pair_bound`]）：先遇到
//!   它就说明当前这个开标签没配对，整块丢弃。同一个 HTML 块里连写的开标签仍按块内规则处理：先写的
//!   赢，内容到第一个 `</p>` 为止，后面的开标签落在内容里（那种以标签开头的行是一整块原始 HTML，
//!   连它后面的文字一起丢掉）。
//! - 写法必须精确：标签名、属性个数、引号与大小写的写法都算（见 [`open_tag`]），闭标签必须
//!   恰好是 `</p>`。
//!
//! 行内 HTML 只认精确的 `<kbd>` 与 `</kbd>` 这两个标签（见 [`KbdState`]）：它们不产出片段，
//! 只把两个标签之间产出的片段标成同一段 kbd 内容。其它行内 HTML 与 `<kbd>` 的其它写法
//! （`<KBD>`、带属性、`<kbd/>`）都不影响任何片段。
//!
//! 被丢弃的节点类别包括（非穷举）：`BlockQuote`、`List`、`Item`、`TaskItem`、`CodeBlock`、
//! `Code`、`ThematicBreak`、`Table` / `TableRow` / `TableCell`、`Strikethrough`、`Math`、
//! `FootnoteDefinition` / `FootnoteReference`、`FrontMatter`、`DescriptionList` 系列、
//! `MultilineBlockQuote`、`Raw`、`WikiLink`、`Escaped`、`Superscript`、`Highlight`、`Insert`。
//! `HtmlBlock` 里只有那一种标签对会显示，块里其余内容照旧整块丢弃，原始文本块（`<pre>`、
//! `<script>`、注释…）连标签对都不看。
//!
//! 行内递归还有一层硬性上限：嵌套超过 [`MAX_INLINE_NESTING`] 的行内节点**连同整棵子树丢弃**。
//! 这是防止用户可控输入（例如两侧各几千个 `*`）把递归栈打爆导致进程 abort 的兜底，
//! 真实 markdown 的行内嵌套个位数就够。

use std::ops::Range;

use comrak::{
    Arena, Options,
    nodes::{Node, NodeLink, NodeValue},
};
use gpui_kit::{AssetSource, SharedString};

use super::{
    document::{
        HeadingLevel, KbdId, LinkId, MarkdownBlock, MarkdownDocument, MarkdownPiece, ParagraphAlign,
    },
    source::InlineImageSource,
};

/// 行内节点允许的最大嵌套层数。
const MAX_INLINE_NESTING: usize = 64;
/// comrak 的 HTML 块类型（`NodeHtmlBlock::block_type`）里第一个"原文就是一串标签"的类型。
///
/// 1..=5 分别是 `<pre>` / `<script>` / `<style>` / `<textarea>`、注释、`<?…?>`、`<!…>` 与
/// `<![CDATA[…]]>`：它们的原文是**原始文本**，里面的 `<p …>` 不是元素，所以整块不看。
/// 6（一行以块级标签开头）与 7（一行是一个完整标签）才可能有我们的标签对。
const FIRST_TAG_BLOCK_TYPE: u8 = 6;
/// 对齐标签的闭标签。
const ALIGN_TAG_CLOSE: &str = "</p>";
/// HTML 注释的开头与结尾：注释里的 `<` / `>` / 引号都不算标签的一部分。
const COMMENT_OPEN: &str = "<!--";
const COMMENT_CLOSE: &str = "-->";

/// 原文的行索引：每行的起点，以及这一行是不是空行。
///
/// 行尾口径与 comrak 一致（`\n`、`\r\n`、`\r` 都是行尾），所以 `sourcepos` 里的行号可以直接拿来
/// 索引这里。行号一律 1-based；行尾换行符本身不属于任何一行。
struct SourceLines {
    /// 每行的起点字节偏移。
    starts: Vec<usize>,
    /// 每行是不是空行（除空白外什么都没有）。
    blank: Vec<bool>,
}

impl SourceLines {
    fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut starts = vec![0];
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                    index += 2;
                    starts.push(index);
                }
                b'\r' | b'\n' => {
                    index += 1;
                    starts.push(index);
                }
                _ => index += 1,
            }
        }

        let blank = (0..starts.len())
            .map(|line| {
                let start = starts[line];
                let end = starts.get(line + 1).copied().unwrap_or(source.len());
                source[start..end].trim().is_empty()
            })
            .collect();

        Self { starts, blank }
    }

    /// 1-based 行号那一行的起点字节偏移；行号越界（`sourcepos` 不会给出这种值）时给 `None`。
    fn line_offset(&self, line: usize) -> Option<usize> {
        self.starts.get(line.checked_sub(1)?).copied()
    }

    /// 字节偏移落在哪一行（1-based）。
    fn line_of(&self, offset: usize) -> usize {
        self.starts.partition_point(|start| *start <= offset)
    }

    /// 两个行号之间（不含两端的行）有几行空行。
    fn blank_between(&self, previous_end: usize, next_start: usize) -> usize {
        (previous_end..next_start.saturating_sub(1))
            .filter(|index| self.blank.get(*index).copied().unwrap_or(false))
            .count()
    }
}

/// 递归下传的行内样式上下文。字段是 `pub`、类型只是 `pub(super)`：
/// 这样 `parse_tests` 能构造它，而 crate 之外完全看不见它
/// （字段上不能写 `pub(super)`，那条被 `field_scoped_visibility_modifiers` 禁止）。
#[derive(Clone, Copy, Default, PartialEq)]
pub(super) struct PieceStyle {
    pub strong: bool,
    pub emphasis: bool,
    pub link: Option<LinkId>,
    pub kbd: Option<KbdId>,
}

/// 解析一个块时的 kbd 标签状态。
///
/// comrak 把 `<kbd>` 与 `</kbd>` 解析成两个互不相干的 `HtmlInline` 节点，这里只按节点顺序
/// 增减深度：最外层的一对标签打开时分配一个 [`KbdId`]，其中的内容共用这个编号，关闭时清掉。
/// 未配对的标签不做额外校验——孤立的 `</kbd>` 是空操作，未闭合的 `<kbd>` 一直管到块尾，
/// 嵌套的标签只算最外层那一个框（编号见 [`KbdId`]）。标签不可能跨块配对，所以状态按块创建。
struct KbdState {
    /// 当前打开的 kbd 标签层数。
    depth: usize,
    /// 最外层那一对标签的编号；不在 kbd 里时为 `None`。
    current: Option<KbdId>,
    /// 下一个可用的编号。
    next: usize,
}

impl KbdState {
    fn new() -> Self {
        Self {
            depth: 0,
            current: None,
            next: 0,
        }
    }

    fn open(&mut self) {
        if self.depth == 0 {
            self.current = Some(KbdId(self.next));
            self.next += 1;
        }
        self.depth += 1;
    }

    fn close(&mut self) {
        if self.depth == 0 {
            return;
        }
        self.depth -= 1;
        if self.depth == 0 {
            self.current = None;
        }
    }
}

/// 解析过程中的文字缓冲：一段样式相同的文字只在这里累加，最后一次性落盘。
/// 与 `PieceStyle` 同理：类型 `pub(super)`、字段 `pub`，好让 `parse_tests` 直接驱动这两个函数。
pub(super) struct TextBuffer {
    pub text: String,
    pub style: PieceStyle,
}

/// 把一份 markdown 原文解析成 IR。空的段落与标题（例如只含一张被忽略的图片）不入 `blocks`。
///
/// `assets` 只用来读资源：`![…](file://…)` 没写 `size` 时，图片的绘制尺寸取自资源自身的尺寸
/// （见 [`InlineImageSource::resolve`]）。解析只发生一次，所以这次读资源也是每个文档一次。
pub(super) fn parse_document(source: &str, assets: &dyn AssetSource) -> MarkdownDocument {
    let lines = SourceLines::new(source);
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, source, &Options::default());
    let nodes = root.children().collect::<Vec<_>>();
    let open_tags = open_tags(&nodes, source, &lines);
    let mut blocks = Vec::new();
    let mut links = Vec::new();
    // 上一个真的产出块的块级结构结束在哪一行：块与块之间的空行按它与下一块的行号算。
    let mut previous_end = None;
    // 配成的标签对把原文的哪个偏移之前的一切吃进内容里了：起点落在它之前的节点不再各自成块。
    let mut consumed_before = 0;
    // 候选开标签的游标：候选按原文顺序，节点也按原文顺序，所以一个游标就够。
    let mut next_open = 0;

    for node in &nodes {
        let data = node.data();
        // 起点落在已配成的标签对内容里的节点不再各自成块。HTML 块是例外：它自己什么都不画
        // （只有它的标签对算数，而哪几对落在内容里由候选游标按偏移判断），一个 HTML 块里
        // 闭标签**之后**还写着下一个开标签时，那一对不能被吃掉它的闭标签一起丢掉。
        let consumed = lines
            .line_offset(data.sourcepos.start.line)
            .is_some_and(|start| start < consumed_before);
        if consumed && !matches!(data.value, NodeValue::HtmlBlock(_)) {
            continue;
        }

        match &data.value {
            // HTML 块只认 `<p align="…">…</p>` 标签对：一块里可以连续写几对，各产出自己的一串块；
            // 块里其余内容照旧不显示（见 [`align_pair`]）。
            NodeValue::HtmlBlock(html) => {
                // 原始文本块（`<pre>`、`<script>`、注释…）的原文不是标签序列，整块不看，里面
                // 也不会有候选开标签。
                if html.block_type < FIRST_TAG_BLOCK_TYPE {
                    continue;
                }
                let block_end = lines
                    .line_offset(data.sourcepos.end.line + 1)
                    .unwrap_or(source.len());
                while let Some(tag) = open_tags.get(next_open)
                    && tag.tag.start < block_end
                {
                    let index = next_open;
                    next_open += 1;
                    // 已经配成的那一对把这一段吃进内容里了（同一个 HTML 块里的嵌套开标签）。
                    if tag.tag.start < consumed_before {
                        continue;
                    }
                    let bound = pair_bound(source, &open_tags, index);
                    let Some(pair) = align_pair(source, &lines, tag, bound) else {
                        // 没配对：这一块照旧整块丢弃，后面配好的标签对不受影响。
                        continue;
                    };
                    consumed_before = pair.end;
                    let blocks_of_pair = pair_blocks(
                        &source[pair.content.clone()],
                        pair.align,
                        assets,
                        &mut links,
                    );
                    if blocks_of_pair.is_empty() {
                        continue;
                    }
                    push_blank_lines(
                        &lines,
                        previous_end,
                        lines.line_of(tag.tag.start),
                        &mut blocks,
                    );
                    blocks.extend(blocks_of_pair);
                    previous_end = Some(pair.close_line);
                }
            }
            _ => {
                let Some((block, end)) =
                    content_block(node, ParagraphAlign::Left, assets, &mut links)
                else {
                    continue;
                };
                push_blank_lines(&lines, previous_end, data.sourcepos.start.line, &mut blocks);
                blocks.push(block);
                previous_end = Some(end);
            }
        }
    }

    MarkdownDocument {
        blocks,
        links: links.into(),
    }
}

/// 一个内容型块级节点产出的块，以及它结束在哪一行。
///
/// 段落用 `align`（顶层是左对齐，标签对里是标签对的对齐），标题照旧是标题；不产出块的节点
/// （列表、引用、代码块、HTML 块、空的段落…）给 `None`。顶层循环与 [`pair_blocks`] 共用这一份，
/// "哪些节点算内容"因此不会在两处漂移。
fn content_block(
    node: Node<'_>,
    align: ParagraphAlign,
    assets: &dyn AssetSource,
    links: &mut Vec<SharedString>,
) -> Option<(MarkdownBlock, usize)> {
    let data = node.data();
    let level = match &data.value {
        NodeValue::Heading(heading) => Some(HeadingLevel::from_u8(heading.level)),
        NodeValue::Paragraph => None,
        _ => return None,
    };
    let pieces = block_pieces(node, assets, links);
    if pieces.is_empty() {
        // 空的段落与标题（例如只含一张被忽略的图片）不入 `blocks`。
        return None;
    }
    let block = match level {
        Some(level) => MarkdownBlock::Heading {
            level,
            pieces: pieces.into(),
        },
        None => MarkdownBlock::Paragraph {
            align,
            pieces: pieces.into(),
        },
    };

    Some((block, data.sourcepos.end.line))
}

/// 补上上一个块与下一个块之间的空行（一个空行一个 [`MarkdownBlock::BlankLine`]）。
///
/// `previous_end` 是上一个真的产出块的块级结构结束的行号；还没有块时（文档开头）什么也不补：
/// 文档首尾的空行不画。
fn push_blank_lines(
    lines: &SourceLines,
    previous_end: Option<usize>,
    next_start: usize,
    blocks: &mut Vec<MarkdownBlock>,
) {
    let Some(previous_end) = previous_end else {
        return;
    };
    blocks.extend(std::iter::repeat_n(
        MarkdownBlock::BlankLine,
        lines.blank_between(previous_end, next_start),
    ));
}

/// 顶层 HTML 块里的一个候选开标签：`<p align="…">`。
struct OpenTag {
    /// 开标签在原文里的范围（`<` 与配对的 `>` 都在内）。
    tag: Range<usize>,
    /// 开标签给出的对齐方式。
    align: ParagraphAlign,
    /// 开标签所在 HTML 块的结束行（1-based）。
    ///
    /// 只用来判断"下一个候选是不是跨出了本块"：块内的开标签按块内规则处理（先写的赢），跨块的
    /// 那一个（如果有）决定往后找闭标签能走多远。
    block_end_line: usize,
}

/// 一个候选开标签配上对之后的结果。
struct AlignPair {
    /// 对齐方式，取自开标签。
    align: ParagraphAlign,
    /// 标签对之间的内容在原文里的范围。
    content: Range<usize>,
    /// 闭标签之后的偏移：从这里往后才轮到后面的原文。
    end: usize,
    /// 闭标签所在行（1-based），用来记下这个段落结束在哪一行。
    close_line: usize,
}

/// 顶层 HTML 块里的所有候选开标签，按原文顺序。
///
/// 只有 `block_type >= FIRST_TAG_BLOCK_TYPE` 的 HTML 块里的开标签才算候选，"开标签必须在顶层
/// HTML 块里"这条契约因此在这里落实：列表项、引用、缩进代码块与段落中间的行内 HTML 都不在候选里
/// （那几种行不是 HTML 块）。
///
/// 块里的开标签写在行首还是跟在别的标签后面都算（`<div>x <p align="center">y</p></div>` 里那个
/// 也是元素）；它跨空行往后配对时，被它跨过的段落与标题照旧照常画出来（见 [`pair_blocks`]），
/// 所以"从行中间开始的标签对"不会让后面的内容消失。
fn open_tags(nodes: &[Node<'_>], source: &str, lines: &SourceLines) -> Vec<OpenTag> {
    let mut open_tags = Vec::new();
    for node in nodes {
        let data = node.data();
        let NodeValue::HtmlBlock(html) = &data.value else {
            continue;
        };
        if html.block_type < FIRST_TAG_BLOCK_TYPE {
            continue;
        }
        let Some(start) = lines.line_offset(data.sourcepos.start.line) else {
            continue;
        };
        let end = lines
            .line_offset(data.sourcepos.end.line + 1)
            .unwrap_or(source.len());
        let mut rest = &source[start..end];
        // `rest` 一段段往后走，`base` 记着它相对原文的起点：`next_tag` 给的范围是相对 `rest` 的。
        let mut base = start;
        while let Some(tag) = next_tag(rest) {
            let inner = &rest[tag.start + 1..tag.end];
            if let Some(align) = open_tag(inner.strip_suffix('>').unwrap_or(inner)) {
                open_tags.push(OpenTag {
                    tag: base + tag.start..base + tag.end,
                    align,
                    block_end_line: data.sourcepos.end.line,
                });
            }
            base += tag.end;
            rest = &rest[tag.end..];
        }
    }

    open_tags
}

/// 第 `index` 个候选往后找闭标签的终点：下一个**跨出本块**的候选开标签，没有就是原文结尾。
///
/// 这就是"一个没配对的开标签不能把后面的文案一起吞掉"的那道防线：跨空行往后找时，先遇到下一个
/// HTML 块里的候选开标签，说明当前这个没配对（见 [`align_pair`]）。同一个块里的候选不算终点——块内
/// 按"内容到第一个 `</p>` 为止"的老规则处理。
fn pair_bound(source: &str, open_tags: &[OpenTag], index: usize) -> usize {
    let tag = &open_tags[index];
    open_tags[index + 1..]
        .iter()
        .find(|next| next.block_end_line > tag.block_end_line)
        .map_or(source.len(), |next| next.tag.start)
}

/// 给一个候选开标签找闭合标签（`bound` 是终点，见 [`pair_bound`]），配上对就交出内容与结束位置。
///
/// 闭标签是第一个不在注释里的 `</p>`（见 [`find_close_tag`]），中间可以有空行。找不到就是没配对。
fn align_pair(source: &str, lines: &SourceLines, tag: &OpenTag, bound: usize) -> Option<AlignPair> {
    let close = tag.tag.end + find_close_tag(&source[tag.tag.end..bound])?;

    Some(AlignPair {
        align: tag.align,
        content: tag.tag.end..close,
        end: close + ALIGN_TAG_CLOSE.len(),
        close_line: lines.line_of(close),
    })
}

/// 解析一个块级节点（标题 / 段落）的行内片段。
fn block_pieces(
    node: Node<'_>,
    assets: &dyn AssetSource,
    links: &mut Vec<SharedString>,
) -> Vec<MarkdownPiece> {
    let mut pieces = Vec::new();
    parse_pieces(
        node,
        PieceStyle::default(),
        0,
        &mut KbdState::new(),
        assets,
        links,
        &mut pieces,
    );
    pieces
}

/// `text` 里结束标签对的那个 `</p>` 的起点；没有就返回 `None`。
///
/// 按字节找，不要求它自己是一个"合法标签"：正文里的小于号（`if a<b then c`）不该把闭合标签
/// 一起吞掉。唯一的例外是注释：`<!--` … `-->` 里的 `</p>` 不是标签（`a<!-- </p> -->b</p>`
/// 里真正的闭合标签是最后一个），整段注释跳过——注释没闭合时它一直到结尾，函数返回 `None`。
/// 返回 `None` 因此意味着"传进来的这段原文里没有能闭合的 `</p>`"，调用方据此判定这个开标签
/// 没配对即可（`text` 是**有终点**的一段：见 [`pair_bound`]）。
///
/// 代价是按字节找就分不清"被引号包住的属性值里的 `</p>`"（内容里写 `<span title="</p>">`
/// 时会在那里闭合）。那不是能显示的 HTML——本组件本来就不画行内 HTML——而跟踪引号状态要冒
/// 更大的风险：正文里一个撇号（`it's`）就会被当成没闭合的引号，让它后面的标签对全部消失。
fn find_close_tag(text: &str) -> Option<usize> {
    let mut rest = text;
    let mut base = 0;
    while let Some(offset) = rest.find(ALIGN_TAG_CLOSE) {
        let Some(comment) = rest[..offset].rfind(COMMENT_OPEN) else {
            return Some(base + offset);
        };
        // 这一处 `</p>` 落在注释里：跳到注释之后接着找。
        let end = comment + comment_end(&rest[comment..]);
        base += end;
        rest = &rest[end..];
    }
    None
}

/// 从 `<!--` 开始的一段注释到哪里结束（返回值是**注释之后**的长度，相对传入的 `text`）。
///
/// `text` 必须正好从 `<!--` 开始。HTML 允许 `<!-->` 与 `<!--->` 这两种空注释，它们在 `>` 处
/// 就结束；其余注释到 `-->` 为止，没有 `-->` 就是没闭合的注释，一直到结尾。
fn comment_end(text: &str) -> usize {
    let body = &text[COMMENT_OPEN.len()..];
    match body.as_bytes() {
        [b'>', ..] => COMMENT_OPEN.len() + 1,
        [b'-', b'>', ..] => COMMENT_OPEN.len() + 2,
        _ => body.find(COMMENT_CLOSE).map_or(text.len(), |offset| {
            COMMENT_OPEN.len() + offset + COMMENT_CLOSE.len()
        }),
    }
}

/// `text` 里下一个 HTML 标签的范围（`<` 与配对的 `>` 都在内）。
///
/// 只做词法，不做 HTML 解析：`<` 后面必须紧跟字母、`/` + 字母或 `!`（`<div>`、`</p>`、`<!--`），
/// 否则那个 `<` 只是正文里的小于号（`<3`）。标签里被引号包围的属性值整体跳过，所以属性值里的
/// `<p …>` 不构成一个标签。注释整体算一个"标签"（到哪结束见 [`comment_end`]），于是被注释掉的
/// 标签对不会被当成元素——注释里的引号（`don't`）与 `>` 也就不会把后面的原文带偏。
fn next_tag(text: &str) -> Option<Range<usize>> {
    let mut from = 0;
    while let Some(offset) = text[from..].find('<') {
        let start = from + offset;
        let rest = &text[start + 1..];
        // 注释整体算一个"标签"：被注释掉的标签对不是元素，注释里的引号（`don't`）与 `>` 也就
        // 不会把后面的原文带偏（注释到哪里结束见 [`comment_end`]）。
        if rest.starts_with("!--") {
            return Some(start..start + comment_end(&text[start..]));
        }
        let opens_tag = match rest.chars().next() {
            Some(character) if character.is_ascii_alphabetic() => true,
            Some('!') => true,
            Some('/') => rest[1..]
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic()),
            _ => false,
        };
        if !opens_tag {
            from = start + 1;
            continue;
        }

        let mut quote = None;
        for (offset, character) in rest.char_indices() {
            match quote {
                Some(open) => {
                    if character == open {
                        quote = None;
                    }
                }
                None => match character {
                    '"' | '\'' => quote = Some(character),
                    '>' => return Some(start..start + offset + 2),
                    _ => {}
                },
            }
        }
        // 没有配对的 `>`：从那个 `<` 到结尾都构不成标签，往后也不必再找。
        return None;
    }
    None
}

/// 解析一个标签里 `<` 与 `>` 之间的部分：标签名必须恰好是 `p`，属性只允许一个可选的
/// `align="left|right|center"`。
///
/// 返回对齐方式；写法不合法时返回 `None`（这一对标签因此不生效）。判定严格到字符：`p`、
/// `p `、`p align="center"` 合法；`pre`、`param` 之类的别的标签名、`p align='center'`、
/// `p align=center`、`p align = "center"`、`p class="x"`、第二个属性、重复的 `align` 与
/// `justify` 之类的其它取值都不合法。
fn open_tag(inner: &str) -> Option<ParagraphAlign> {
    let mut rest = inner.strip_prefix('p')?;
    if !rest.is_empty() && !rest.starts_with(|character: char| character.is_ascii_whitespace()) {
        return None;
    }

    let mut align = None;
    loop {
        rest = rest.trim_start_matches(|character: char| character.is_ascii_whitespace());
        if rest.is_empty() {
            return Some(align.unwrap_or_default());
        }
        if align.is_some() {
            return None;
        }
        let after = rest.strip_prefix("align=")?.strip_prefix('"')?;
        let end = after.find('"')?;
        align = Some(match &after[..end] {
            "left" => ParagraphAlign::Left,
            "right" => ParagraphAlign::Right,
            "center" => ParagraphAlign::Center,
            _ => return None,
        });
        rest = &after[end + 1..];
    }
}

/// 解析 `<p align="…">` 标签对之间的内容，产出这一对给出的块。
///
/// 内容在 CommonMark 里属于原始 HTML，comrak 不会为它做解析，所以这里把它当成一份独立文档重新
/// 解析一次，再按块取用（与顶层同一条规则，见 [`content_block`]）：**段落带上这一对的对齐方式，
/// 标题照旧是标题**（标题在本组件里恒为左对齐），空行照旧产出 [`MarkdownBlock::BlankLine`]；
/// 其余块级结构（代码块、`1. 列表`、`<div>` 之类的 HTML 块…）连同子树一并丢弃——它们在组件的
/// 任何位置都不画。标签对之间因此可以跨行写一整段 markdown，跨过的段落与标题都照常画出来。
///
/// 内容里的行号是相对**内容字符串**的（`SourceLines` 与重新解析的 `sourcepos` 都是），两边的
/// 空行账因此对得上；文档首尾的空行不画这条同样适用，所以内容开头的空行不产出空行块。
fn pair_blocks(
    source: &str,
    align: ParagraphAlign,
    assets: &dyn AssetSource,
    links: &mut Vec<SharedString>,
) -> Vec<MarkdownBlock> {
    let lines = SourceLines::new(source);
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, source, &Options::default());
    let mut blocks = Vec::new();
    // 上一个真的产出块的块级结构结束在哪一行：块与块之间的空行按它与下一块的行号算。
    let mut previous_end = None;

    for node in root.children() {
        let Some((block, end)) = content_block(node, align, assets, links) else {
            continue;
        };
        push_blank_lines(
            &lines,
            previous_end,
            node.data().sourcepos.start.line,
            &mut blocks,
        );
        blocks.push(block);
        previous_end = Some(end);
    }

    blocks
}

/// 递归收集行内片段；`nesting` 是当前的行内嵌套层数，超过 [`MAX_INLINE_NESTING`] 时
/// 连同整棵子树丢弃——不 panic、也不再往下递归，用户可控的深层嵌套因此打不爆递归栈。
fn parse_pieces(
    node: Node<'_>,
    style: PieceStyle,
    nesting: usize,
    kbd: &mut KbdState,
    assets: &dyn AssetSource,
    links: &mut Vec<SharedString>,
    out: &mut Vec<MarkdownPiece>,
) {
    if nesting > MAX_INLINE_NESTING {
        return;
    }
    let mut buffer = None;
    for child in node.children() {
        // 产出片段时才现取 kbd 编号：`<kbd>` / `</kbd>` 是彼此独立的节点，状态是唯一真相来源，
        // 递归进 Strong / Emph / Link 也不丢（标签跨过标记边界时照样按节点顺序生效）。
        let style = PieceStyle {
            kbd: kbd.current,
            ..style
        };
        match &child.data().value {
            NodeValue::HtmlInline(tag) => match tag.as_str() {
                "<kbd>" => kbd.open(),
                "</kbd>" => kbd.close(),
                _ => {}
            },
            NodeValue::Text(text) => push_text(&mut buffer, out, text, &style),
            NodeValue::Strong => {
                flush(&mut buffer, out);
                parse_pieces(
                    child,
                    PieceStyle {
                        strong: true,
                        ..style
                    },
                    nesting + 1,
                    kbd,
                    assets,
                    links,
                    out,
                );
            }
            NodeValue::Emph => {
                flush(&mut buffer, out);
                parse_pieces(
                    child,
                    PieceStyle {
                        emphasis: true,
                        ..style
                    },
                    nesting + 1,
                    kbd,
                    assets,
                    links,
                    out,
                );
            }
            NodeValue::Link(link) => {
                flush(&mut buffer, out);
                parse_link(child, &link.url, style, nesting, kbd, assets, links, out);
            }
            NodeValue::Image(image) => {
                flush(&mut buffer, out);
                parse_image(image, &style, assets, out);
            }
            NodeValue::SoftBreak | NodeValue::LineBreak => {
                flush(&mut buffer, out);
                out.push(MarkdownPiece::LineBreak);
            }
            _ => {}
        }
    }
    flush(&mut buffer, out);
}

/// 收集链接内容；只有确实产出了片段时才把它记进 `links`，`LinkId` 因此不会指向没人用的 URL。
fn parse_link(
    node: Node<'_>,
    url: &str,
    style: PieceStyle,
    nesting: usize,
    kbd: &mut KbdState,
    assets: &dyn AssetSource,
    links: &mut Vec<SharedString>,
    out: &mut Vec<MarkdownPiece>,
) {
    let mut inner = Vec::new();
    parse_pieces(node, style, nesting + 1, kbd, assets, links, &mut inner);
    if inner.is_empty() {
        return;
    }
    let id = LinkId(links.len());
    links.push(SharedString::from(url));
    for piece in inner {
        out.push(piece.with_link(id));
    }
}

/// 只接受 `file:` URI（语法见 [`super::source`]）；其它图片一律不绘制，连 alt 文本也不画。
///
/// 尺寸在这里一次定下来：写了 `size` 就是它，没写就读一次资源问它自己有多大——两张同样写法的
/// 图片因此不会在绘制阶段各问一次，折行与定位拿到的也只可能是同一个数。
fn parse_image(
    image: &NodeLink,
    style: &PieceStyle,
    assets: &dyn AssetSource,
    out: &mut Vec<MarkdownPiece>,
) {
    let Some(source) = InlineImageSource::parse(&image.url) else {
        return;
    };
    let size = source.resolve(assets);
    out.push(MarkdownPiece::InlineImage {
        path: SharedString::from(source.path),
        format: source.format,
        size,
        link: style.link,
        kbd: style.kbd,
    });
}

/// 把 `text` 追加进待落盘的文字缓冲，样式不同时先把缓冲落盘。
pub(super) fn push_text(
    buffer: &mut Option<TextBuffer>,
    out: &mut Vec<MarkdownPiece>,
    text: &str,
    style: &PieceStyle,
) {
    if text.is_empty() {
        return;
    }
    match buffer {
        Some(current) if current.style == *style => current.text.push_str(text),
        _ => {
            flush(buffer, out);
            *buffer = Some(TextBuffer {
                text: text.to_string(),
                style: *style,
            });
        }
    }
}

/// 把待落盘的文字缓冲变成一个 [`MarkdownPiece::Text`]；没有缓冲时什么也不做。
pub(super) fn flush(buffer: &mut Option<TextBuffer>, out: &mut Vec<MarkdownPiece>) {
    let Some(current) = buffer.take() else {
        return;
    };
    out.push(MarkdownPiece::Text {
        text: SharedString::from(current.text),
        strong: current.style.strong,
        emphasis: current.style.emphasis,
        link: current.style.link,
        kbd: current.style.kbd,
    });
}

//! 行内流动元素的行为测试：折行、kbd 段的不折行、图片对行高的影响、链接点击、hover 与状态复用。
//!
//! 断言只走两条路：渲染结果（`debug_bounds` 的调试选择器）与 `MarkdownState` 上的方法。
//! 几何值全部在**同一个 harness 内部**做相对比较（`a > b`）或只比较 `Some` / `None`，
//! 不硬编码像素值：手推像素偏移不可靠。

use std::{cell::Cell, rc::Rc};

use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, ImageSource, InteractiveElement as _, IntoElement,
    Modifiers, MouseButton, ParentElement, Pixels, Point, Render, Resource, SharedString,
    Styled as _, TestAppContext, VisualTestContext, Window, div, point, px,
};

use super::{
    document::LinkId,
    view::{Markdown, MarkdownState},
};
use crate::style;

/// 窄容器：长文字在这里必须折行，图片也大到足以改变行高。
const NARROW: Pixels = px(220.);
/// 在 [`NARROW`] 里必定折行的纯文字。
const LONG_TEXT: &str = "The quick brown fox jumps over the lazy dog and keeps on running well \
                         past the right edge of this narrow container.";
/// 同一 harness 里的对照：一行就放得下。
const SHORT_TEXT: &str = "Short.";
/// 在 [`NARROW`] 里必定折行的中文长段落：整段没有空格，只能逐字断行。
const LONG_CHINESE: &str = "这是一段足够长的中文测试文本用来验证折行逻辑并确认它会被排成多行";
/// 同一 harness 里的对照：一行就放得下的短中文。
const SHORT_CHINESE: &str = "短文本";
/// 唯一一个链接的段落，链接文字从块左上角开始。
const LINK_TEXT: &str = "[site](https://example.com)";
/// [`LINK_TEXT`] 指向的 URL。
const LINK_URL: &str = "https://example.com";
/// 一张行内图片：默认 16px 的箭头 svg（`svg` 没有固有尺寸，所以不写 `size` 就是 16px）。
const IMAGE: &str = "![a](file://icons/phosphoricons/arrow-up.svg)";
/// 同一张图，写成 24px：窄容器里它比默认的那张多折一行。
const WIDE_IMAGE: &str = "![a](file://icons/phosphoricons/arrow-up.svg?size=24)";
/// 同一张图，写成 40px：它比默认行高还高。
const TALL_IMAGE: &str = "![a](file://icons/phosphoricons/arrow-up.svg?size=40)";
/// 同一张图，宽高分开写：宽度与默认的一样，只有高度是 64px。
const RECTANGLE_IMAGE: &str = "![a](file://icons/phosphoricons/arrow-up.svg?size=16,64)";
/// 占位文案里那张「市场」图标（`common.market_not_loaded` 用的就是它）。
const GRAPH_IMAGE: &str = "![a](file://icons/phosphoricons/graph.svg)";
/// 一张宽到 kbd 段自己就放不下的图：整段只能移到下一行。
const WIDE_KBD_IMAGE: &str = "![a](file://icons/phosphoricons/graph.svg?size=200)";
/// 链接里只有一张图片：链接的命中盒必须把它也算进去。
const LINKED_IMAGE: &str = "[![a](file://icons/phosphoricons/arrow-up.svg)](https://example.com)";
/// 链接里那张 24px 的图：在 [`NARROW`] 里它一定被折到下一行。
const LINKED_WIDE_IMAGE: &str =
    "[![a](file://icons/phosphoricons/arrow-up.svg?size=24)](https://example.com)";
/// 两张会被整张丢掉的图片被空格包住：丢掉之后它们两侧的空格变成**相邻的两个片段**。
///
/// 这是 `place_lines` 那次减法下溢的触发形状：折行点落进这两个空格之间时，行尾悬挂的空白
/// 整段落在第二个片段之前，第二个片段在可见范围里一个字符都不占。
const DROPPED_IMAGES_BETWEEN_SPACES: &str = "An unknown type ![](https://example.com/a.png) and another one \
     ![](https://example.com/b.png) are dropped whole.";
/// example 里真正触发它的那一段：两张被丢掉的图片把两个空格拼到一起，第三张图留下一个 16px 的
/// 占位框（`file://nope/nothing.png` 加载不出来，按兜底尺寸画）。
const DROPPED_IMAGE_PARAGRAPH: &str = "An unknown type ![a](file://icons/phosphoricons/arrow-up.svg?type=ico) \
     and an image that is not a file URI at all ![a](https://example.com/a.png) are ignored whole, alt text \
     included, so this sentence closes the gap they leave; an asset that is simply not there \
     ![a](file://nope/nothing.png) keeps the 16-pixel box the fallback size gives it, so a hole stays where \
     the words around it part.";
/// 块选择器：`debug_selector` 记的是 `String`，查询只能用字面量。
const BLOCK_0: &str = "BASE_UI_MARKDOWN_BLOCK_0";
const BLOCK_1: &str = "BASE_UI_MARKDOWN_BLOCK_1";
/// 第一个空行的选择器：空行是 `blocks` 里的一员，画成一行正文行高。
const BLANK_0: &str = "BASE_UI_MARKDOWN_BLANK_0";
/// 两个实例的外层包装；两个实例都没有内边距，所以包装的左上角就是各自块 0 的左上角。
const FIRST_COPY: &str = "BASE_UI_MARKDOWN_FIRST_COPY";
const SECOND_COPY: &str = "BASE_UI_MARKDOWN_SECOND_COPY";

/// 持有 `Entity<MarkdownState>` 的最小 `Render` 视图。
struct Harness {
    state: Entity<MarkdownState>,
    /// 包住 markdown 的外层容器宽度；`None` 表示占满窗口。
    width: Option<Pixels>,
}

impl Harness {
    fn new(cx: &mut Context<Self>, source: &str, width: Option<Pixels>) -> Self {
        Self {
            state: cx.new(|cx| MarkdownState::new(source, cx)),
            width,
        }
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let container = match self.width {
            Some(width) => div().w(width),
            None => div().w_full(),
        };

        container.p_2().child(Markdown::new(&self.state))
    }
}

fn harness<'a>(
    cx: &'a mut TestAppContext,
    source: &str,
    width: Option<Pixels>,
) -> (Entity<Harness>, &'a mut VisualTestContext) {
    cx.update(style::theme::install);
    let (view, cx) = cx.add_window_view(|_, cx| Harness::new(cx, source, width));
    cx.run_until_parked();

    (view, cx)
}

/// 把同一个 `Entity<MarkdownState>` 渲染两遍的视图，用来验证 hover 与点击在实例之间的隔离。
struct TwoCopies {
    state: Entity<MarkdownState>,
}

impl Render for TwoCopies {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // 包装没有内边距：包装的左上角就是它里面块 0 的左上角。
        div()
            .w_full()
            .p_2()
            .child(
                div()
                    .debug_selector(|| String::from(FIRST_COPY))
                    .child(Markdown::new(&self.state)),
            )
            .child(
                div()
                    .debug_selector(|| String::from(SECOND_COPY))
                    .child(Markdown::new(&self.state)),
            )
    }
}

fn two_copies<'a>(
    cx: &'a mut TestAppContext,
    source: &str,
) -> (Entity<TwoCopies>, &'a mut VisualTestContext) {
    cx.update(style::theme::install);
    let (view, cx) = cx.add_window_view(|_, cx| TwoCopies {
        state: cx.new(|cx| MarkdownState::new(source, cx)),
    });
    cx.run_until_parked();

    (view, cx)
}

/// 换掉整个文档，并等解析与重绘都完成。
fn set_source(cx: &mut VisualTestContext, view: &Entity<Harness>, source: &str) {
    view.update(cx, |harness, cx| {
        harness
            .state
            .update(cx, |state, cx| state.set_source(source, cx));
    });
    settle(cx);
}

/// 改外层容器的宽度并重绘：等价于拖动窗口边缘（measured layout 闭包拿到新的 `Definite` 宽度）。
fn resize(cx: &mut VisualTestContext, view: &Entity<Harness>, width: Pixels) {
    view.update(cx, |harness, cx| {
        harness.width = Some(width);
        cx.notify();
    });
    settle(cx);
}

/// `debug_bounds` 只在真正绘制过的那一帧有值，所以读之前先强制一次完整重画。
fn settle(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
    settle(cx);
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should be drawn"))
}

/// 一个块内的点位：直接从块几何算，不要用 `center()`（链接文字只占块左边一小段，
/// 块中心通常落在文字之外的空白上，那里不命中任何链接）。
fn inside_block(bounds: Bounds<Pixels>) -> Point<Pixels> {
    bounds.origin + point(px(2.), px(2.))
}

/// 状态实体当前标记为 hover 的链接。
fn hovered_link(cx: &VisualTestContext, view: &Entity<Harness>) -> Option<LinkId> {
    cx.read(|app| view.read(app).state.read(app).hovered_link())
}

/// 状态实体当前记录的按下点落在哪条链接上（`None` 表示没有按下点，或按在空白上）。
fn pressed_link(cx: &VisualTestContext, view: &Entity<Harness>) -> Option<LinkId> {
    cx.read(|app| {
        view.read(app)
            .state
            .read(app)
            .pressed()
            .and_then(|pressed| pressed.link)
    })
}

/// 折行点上的空白**悬挂**在上一行行尾，不单独占一行。
///
/// 折行器把空白本身也当成一个折行候选，所以折行点会落在空格之前；`wrap_ranges` 里
/// `hang_trailing_whitespace` 把折行点向后扩到空白串末尾，那个空格就留在上一行行尾，
/// 续行以 `a` 开头。否则空格会落到续行行首、被折行器再当成一次折行点，自己排成一行。
///
/// 这与加粗无关，也不覆盖 Pass C：`wrap_ranges` 里"塑形宽度复核 + 收紧预算"的回退循环在
/// 测试用的 `NoopTextSystem` 下永不执行（该循环当前零自动化覆盖），
/// 整段删掉它这条测试照样通过。本仓库测试主题实测：下面两条都是 2 行 52px；不悬挂行尾空白时
/// 前者是 3 行 78px。
#[gpui_kit::test]
fn a_whitespace_at_the_wrap_point_should_not_take_a_line_of_its_own(cx: &mut TestAppContext) {
    let (view, cx) = harness(
        cx,
        &format!("**{} {}**", "a".repeat(21), "a".repeat(21)),
        Some(NARROW),
    );
    let with_space = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, &format!("**{}**", "a".repeat(42)));
    let without_space = bounds(cx, BLOCK_0).size.height;

    assert_eq!(
        with_space, without_space,
        "the space that ends up at the wrap point must not take a line of its own: {with_space:?} \
         should equal the same text without it, {without_space:?}"
    );
}

/// 折行点落进跨片段的空白串里时，块照旧画出来：窗口一窄不能把定位阶段算崩。
///
/// 触发形状是"上一片段以空白收尾、下一片段又以空白开头"——两张被整张丢掉的图片正好留下这样一对
/// 片段（`"…type "` 与 `" and…"`）。折行点落进这两个空格之间时，行尾悬挂的空白把两个空格都留在
/// 这一行行尾，第二个片段在可见范围里一个字符都不占；定位阶段当时直接做 `visible_end - 段起点`，
/// 而 `then_some` 的参数是**先求值**的，写在条件里也照样算，于是窗口一拖动就
/// `attempt to subtract with overflow`（`examples/markdown.rs` 的文档里就有这一段）。
///
/// 宽度扫描是必需的：折行点是按可用宽度算出来的，只有让折行点真的落进那两个空格之间才会走到
/// 那一步。把定位阶段那次减法改回 `then_some`，这条扫描在第一个宽度就会 panic。
#[gpui_kit::test]
fn a_wrap_point_inside_a_whitespace_run_between_two_pieces_should_not_panic(
    cx: &mut TestAppContext,
) {
    let (view, cx) = harness(cx, DROPPED_IMAGES_BETWEEN_SPACES, Some(px(1.)));

    for source in [DROPPED_IMAGES_BETWEEN_SPACES, DROPPED_IMAGE_PARAGRAPH] {
        set_source(cx, &view, source);

        // 前提：源码里没有连着两个空格，渲染出来的文字里有——两个空格里，一个写在图片前面、
        // 一个写在图片后面，是丢掉的两张图片把它们拼到一起的。这条前提哪天不成立了（比如图片
        // 不再整张丢掉），下面的扫描就不再覆盖那个形状，所以在这里钉住。
        assert!(
            !source.contains("  "),
            "the source of this test keeps the two spaces apart: they must come from the dropped images"
        );
        let text = cx.read(|app| view.read(app).state.read(app).plain_text());
        assert!(
            text.contains("  "),
            "a dropped image leaves both of its neighbouring spaces behind, so the paragraph should \
             read with two in a row: {text}"
        );

        for width in 1..=200 {
            resize(cx, &view, px(width as f32));
            let drawn = bounds(cx, BLOCK_0);
            assert!(
                drawn.size.height > Pixels::ZERO,
                "the paragraph should still be drawn at {width}px wide, got {drawn:?}"
            );
        }
    }
}

/// 窄容器里放不下的文字必须折行：同一个 harness 里换成短文本后块高严格下降。
#[gpui_kit::test]
fn a_paragraph_wider_than_its_container_should_wrap_onto_more_lines(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, LONG_TEXT, Some(NARROW));
    let wrapped = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, SHORT_TEXT);
    let single_line = bounds(cx, BLOCK_0).size.height;

    assert!(
        single_line > Pixels::ZERO,
        "the short paragraph should still occupy a line, got {single_line:?}"
    );
    assert!(
        wrapped > single_line,
        "a paragraph that does not fit {NARROW:?} should wrap onto more lines: {wrapped:?} \
         should be taller than the one-line {single_line:?}"
    );
}

/// 中文长段落同样参与折行：逻辑索引空间按字节推进，中文一个字三个字节也照样落在字符边界上。
///
/// 若索引空间改用 char 计数，切片会直接 panic（`byte index N is not a char boundary`）；
/// 若折行忽略字节长度，两段中文都会是一行、块高相等。
#[gpui_kit::test]
fn a_long_chinese_paragraph_should_wrap_onto_more_lines(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, LONG_CHINESE, Some(NARROW));
    let wrapped = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, SHORT_CHINESE);
    let single_line = bounds(cx, BLOCK_0).size.height;

    assert!(
        single_line > Pixels::ZERO,
        "the short chinese paragraph should still occupy a line, got {single_line:?}"
    );
    assert!(
        wrapped > single_line,
        "a chinese paragraph that does not fit {NARROW:?} should wrap onto more lines: {wrapped:?} \
         should be taller than the one-line {single_line:?}"
    );
}

/// 第一块与第二块之间的垂直间距（`BLOCK_1` 的顶边减去 `BLOCK_0` 的底边）。
///
/// 空行不占块的下标（见 `view` 的 `block_index`），所以换一份源码之后 `BLOCK_1` 仍然是第二块。
fn gap_between_blocks(cx: &mut VisualTestContext) -> Pixels {
    let first = bounds(cx, BLOCK_0);
    let second = bounds(cx, BLOCK_1);

    second.origin.y - (first.origin.y + first.size.height)
}

/// 源码里的空行就是排版里的一行空行：空行自己占一行正文行高，两块之间不再另加段间距。
///
/// 回归：空行曾经只折成 0.5rem 的段间距——分了段，但看起来不像"空了一行"。
#[gpui_kit::test]
fn a_blank_line_between_two_paragraphs_should_be_one_line_tall(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, "a\n\nb", None);
    let first = bounds(cx, BLOCK_0);
    let blank = bounds(cx, BLANK_0);

    assert_eq!(
        blank.size.height, first.size.height,
        "the blank line should be as tall as the one line of text above it: {blank:?} against \
         {first:?}"
    );
    assert_eq!(
        gap_between_blocks(cx),
        blank.size.height,
        "the two paragraphs should be held apart by exactly that blank line, with no paragraph \
         gap added on top of it"
    );
}

/// 连续几个空行就画几行：`a\n\n\n\nb` 的间距是 `a\n\nb` 的三倍。
#[gpui_kit::test]
fn every_blank_line_should_be_drawn(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "a\n\nb", None);
    let one = gap_between_blocks(cx);

    set_source(cx, &view, "a\n\n\n\nb");
    let three = gap_between_blocks(cx);

    assert_eq!(
        three,
        one * 3.,
        "three blank lines should take three times the room one takes: {three:?} against {one:?}"
    );
}

/// 源码里没有空行的相邻两块仍留一段最小间距（`BLOCK_GAP`），不是一行空行。
#[gpui_kit::test]
fn blocks_without_a_blank_line_between_them_keep_the_minimum_gap(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, "# A\nB", None);
    let line = bounds(cx, BLOCK_1).size.height;
    let gap = gap_between_blocks(cx);

    assert!(
        gap > Pixels::ZERO && gap < line,
        "a heading and the paragraph written right under it should keep a gap smaller than a \
         line: {gap:?} against {line:?}"
    );
}

/// 块内的空硬换行占一整行：一个段落里连写两个硬换行（`a\\\n\\\nb`）比两行多出一行。
///
/// 空行本身会结束段落，所以这是块内唯一还会出现空行的写法（普通空行是空行块）。两条对照把
/// "一行行高"钉死：`a` 是一行、`a\nb` 是两行、`a\\\n\\\nb` 是三行。
#[gpui_kit::test]
fn a_blank_hard_line_inside_a_block_should_take_a_line(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "a", None);
    let one_line = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, "a\nb");
    let two_lines = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, "a\\\n\\\nb");
    let three_lines = bounds(cx, BLOCK_0).size.height;

    assert_eq!(
        two_lines,
        one_line * 2.,
        "the two-line control should be two line boxes tall: {two_lines:?} against {one_line:?}"
    );
    assert_eq!(
        three_lines,
        one_line * 3.,
        "the blank hard line inside the block should take exactly one more line: {three_lines:?} \
         against {one_line:?}"
    );
}

/// 标签对里面的空行同样是一行空行：`<p align="center">a\n\nb</p>` 的两段之间正好夹着一个空行块。
///
/// 回归：空行曾经结束标签对所在的 HTML 块，空行**之前**的那半句因此整段消失。
#[gpui_kit::test]
fn a_blank_line_inside_an_align_tag_pair_should_be_drawn(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "<p align=\"center\">a\n\nb</p>", None);
    let first = bounds(cx, BLOCK_0);
    let blank = bounds(cx, BLANK_0);

    assert_eq!(
        blank.size.height, first.size.height,
        "the blank line inside the pair should be as tall as the line above it: {blank:?} against \
         {first:?}"
    );
    assert_eq!(
        gap_between_blocks(cx),
        blank.size.height,
        "the pair's two paragraphs should be held apart by exactly that blank line"
    );

    set_source(cx, &view, "<p align=\"center\">a</p>");

    assert!(
        cx.debug_bounds(BLOCK_1).is_none() && cx.debug_bounds(BLANK_0).is_none(),
        "without the blank line the pair is one paragraph and no blank line at all"
    );
}

/// H1 的字号倍率大于 H2：同一段文字换成 H1 之后块高严格变大。
#[gpui_kit::test]
fn a_level_one_heading_should_be_taller_than_a_level_two_heading(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "# A", None);
    let h1 = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, "## A");
    let h2 = bounds(cx, BLOCK_0).size.height;

    assert!(
        h1 > h2,
        "an H1 should be scaled up more than an H2: {h1:?} should exceed {h2:?}"
    );
}

/// 非首块的标题在块间距之外还有一段额外上间距，段落没有。
#[gpui_kit::test]
fn a_heading_after_the_first_block_should_have_extra_space_above_it(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "# A\n\ntext", None);
    let heading = bounds(cx, BLOCK_0);
    let paragraph = bounds(cx, BLOCK_1);
    let gap_paragraph = paragraph.origin.y - (heading.origin.y + heading.size.height);

    set_source(cx, &view, "# A\n\n# B");
    let heading = bounds(cx, BLOCK_0);
    let second_heading = bounds(cx, BLOCK_1);
    let gap_heading = second_heading.origin.y - (heading.origin.y + heading.size.height);

    assert!(
        gap_heading > gap_paragraph,
        "a heading that is not the first block should pull in extra space above it: \
         {gap_heading:?} should exceed the plain paragraph gap {gap_paragraph:?}"
    );
}

/// 图片的宽度计入行长：窄容器里 18 个 `a` 之后，24px 图片已经放不下、被折到下一行，
/// 而默认的 16px 图片仍然放得下。两张图片都不高于行高，所以两者的高度差只可能来自多折的
/// 那一行——若折行忽略图片宽度，两者都会是 1 行、块高相等，这条断言就会失败。
/// 本仓库测试主题实测：16px 图片 1 行 26px，24px 图片 2 行 52px。
#[gpui_kit::test]
fn an_image_should_count_its_width_towards_the_line_length(cx: &mut TestAppContext) {
    let narrow = format!("aaaaaaaaaaaaaaaaaa {IMAGE}");
    let (view, cx) = harness(cx, &narrow, Some(NARROW));
    let narrow_image = bounds(cx, BLOCK_0).size.height;

    let wide = format!("aaaaaaaaaaaaaaaaaa {WIDE_IMAGE}");
    set_source(cx, &view, &wide);
    let wide_image = bounds(cx, BLOCK_0).size.height;

    assert!(
        wide_image > narrow_image,
        "a 24px image should push its own line where the default 16px image still fits: \
         {wide_image:?} should exceed {narrow_image:?}"
    );
}

/// 比一行文字更高的图片把整行撑高。
#[gpui_kit::test]
fn an_image_taller_than_the_line_should_stretch_that_line(cx: &mut TestAppContext) {
    let tall_source = format!("x{TALL_IMAGE}y");
    let (view, cx) = harness(cx, &tall_source, Some(NARROW));
    let tall = bounds(cx, BLOCK_0).size.height;

    let inline_source = format!("x{IMAGE}y");
    set_source(cx, &view, &inline_source);
    let inline = bounds(cx, BLOCK_0).size.height;

    assert!(
        tall > inline,
        "a 40px image should make its line taller than the default image does: {tall:?} should \
         exceed {inline:?}"
    );
}

/// 宽高分开写时，图片的高度同样参与行高：`size=16,64` 的那张比正方形的高出一截，
/// 而两者的宽度一样（都是 16px），所以行数不变、只有行高变。
#[gpui_kit::test]
fn an_image_with_its_own_height_should_stretch_that_line(cx: &mut TestAppContext) {
    let square = format!("x{IMAGE}y");
    let (view, cx) = harness(cx, &square, Some(NARROW));
    let square_height = bounds(cx, BLOCK_0).size.height;

    let rectangle = format!("x{RECTANGLE_IMAGE}y");
    set_source(cx, &view, &rectangle);
    let rectangle_height = bounds(cx, BLOCK_0).size.height;

    assert!(
        rectangle_height > square_height,
        "a 16×64 image should make its line taller than a 16×16 one: {rectangle_height:?} should \
         exceed {square_height:?}"
    );
}

/// 画法只决定用哪个元素画，不参与几何：同一份资源、同一个尺寸，走 `svg()` 与走 `img()` 必须
/// 占一样的地方。两个方向各一条——真 svg 被要求按位图画（`type=img`），真位图被要求按矢量图画
/// （`type=svg`）。
///
/// 这条也是唯一走到位图支路（`img()`）的渲染用例：其余图片用例给的都是 `.svg` 路径。它钉得住的是
/// "两支路占的地方一样"，钉不住元素自己的 `.w()` / `.h()`——盒子大小来自片段尺寸
/// （`TextFlow::prepaint` 按它给出可用空间），那两行删掉这里也看不出差别。
///
/// 末一条是对照：40px 的盒子必须比默认 16px 的高，否则前两条可能只是"两支路都错成同一个数"。
#[gpui_kit::test]
fn the_drawing_branch_should_not_change_the_room_an_image_takes(cx: &mut TestAppContext) {
    let svg_path = "file://icons/phosphoricons/graph.svg?size=40";
    let (view, cx) = harness(cx, &format!("x![a]({svg_path})y"), Some(NARROW));
    let svg_of_an_svg = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, &format!("x![a]({svg_path}&type=img)y"));
    let bitmap_of_an_svg = bounds(cx, BLOCK_0).size.height;

    assert_eq!(
        svg_of_an_svg, bitmap_of_an_svg,
        "the same 40px svg drawn as a bitmap should take the room the svg takes: {} should equal \
         {}",
        svg_of_an_svg, bitmap_of_an_svg
    );

    let bitmap_path = "file://brands/Binance/binance.png?size=40";
    set_source(cx, &view, &format!("x![a]({bitmap_path})y"));
    let bitmap_of_a_bitmap = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, &format!("x![a]({bitmap_path}&type=svg)y"));
    let svg_of_a_bitmap = bounds(cx, BLOCK_0).size.height;

    assert_eq!(
        bitmap_of_a_bitmap, svg_of_a_bitmap,
        "the same 40px bitmap drawn as an svg should take the room the bitmap takes: {} should \
         equal {}",
        bitmap_of_a_bitmap, svg_of_a_bitmap
    );

    set_source(cx, &view, &format!("x{IMAGE}y"));
    let default_image = bounds(cx, BLOCK_0).size.height;

    assert!(
        bitmap_of_an_svg > default_image,
        "the 40px box should be taller than the default 16px one: {:?} should exceed {:?}",
        bitmap_of_an_svg,
        default_image
    );
}

/// 点中链接文字会打开它的 URL。
#[gpui_kit::test]
fn clicking_a_link_should_open_its_url(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, LINK_TEXT, None);

    let target = inside_block(bounds(cx, BLOCK_0));
    cx.simulate_click(target, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        Some(String::from("https://example.com")),
        "clicking the link text should open the URL it points at"
    );
}

/// 链接里的图片既是链接的一部分，也是可命中的：hover 到它上面要标记 hover，点它要打开链接。
#[gpui_kit::test]
fn an_image_inside_a_link_should_be_hoverable_and_clickable(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, LINKED_IMAGE, None);
    // 图片是块里的第一个片段，从块左上角开始，所以块内的点就落在它身上。
    let target = inside_block(bounds(cx, BLOCK_0));

    cx.simulate_mouse_move(target, None::<MouseButton>, Modifiers::none());
    settle(cx);
    assert!(
        hovered_link(cx, &view).is_some(),
        "the pointer sits on an image that belongs to a link, so the document should report a \
         hovered link"
    );

    cx.simulate_click(target, Modifiers::none());
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        Some(String::from("https://example.com")),
        "clicking an image inside a link should open the URL that link points at"
    );
}

/// 图片折到下一行之后，命中盒跟着它去下一行：第二行左端按下的是一次链接点击，而第一行同一个
/// x 上按下的只是文字。
///
/// 18 个 `a` 之后 24px 的图已经放不下（同 [`an_image_should_count_its_width_towards_the_line_length`]
/// 的那组数字），所以图独占第二行；命中盒若仍留在第一行，后一条断言就会失败。
#[gpui_kit::test]
fn a_wrapped_image_should_keep_its_hit_box_on_the_line_it_wraps_onto(cx: &mut TestAppContext) {
    let (_view, cx) = harness(
        cx,
        &format!("{} {}", "a".repeat(18), LINKED_WIDE_IMAGE),
        Some(NARROW),
    );
    let block = bounds(cx, BLOCK_0);

    // 第一行的左端是第一行文字，不是图片。
    cx.simulate_click(block.origin + point(px(2.), px(2.)), Modifiers::none());
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        None,
        "the first line holds text, so pressing its left edge should not open the image's link"
    );

    // 第二行的左端是折下来的那张图。
    cx.simulate_click(
        block.origin + point(px(2.), block.size.height - px(2.)),
        Modifiers::none(),
    );
    settle(cx);
    assert_eq!(
        cx.opened_url().as_deref(),
        Some(LINK_URL),
        "the image wrapped onto the second line, so its hit box should be down there"
    );
}

/// 点中纯文字不会打开任何 URL：没有链接时 `link_at` 恒为 `None`。
///
/// 这与"注册不注册命中区域"无关：`prepaint` 无条件 `insert_hitbox`、`paint` 无条件注册
/// 三个鼠标回调，有没有链接只影响 `link_at` 的结果。
#[gpui_kit::test]
fn clicking_plain_text_should_open_nothing(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, "just text", None);

    let target = inside_block(bounds(cx, BLOCK_0));
    cx.simulate_click(target, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        None,
        "a paragraph with no link should not open a URL"
    );
}

/// 只有左键才是点击：右键与中键在链接上抬起都不打开 URL。
#[gpui_kit::test]
fn releasing_a_non_left_button_on_a_link_should_open_nothing(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, LINK_TEXT, None);
    let target = inside_block(bounds(cx, BLOCK_0));

    for button in [MouseButton::Right, MouseButton::Middle] {
        cx.simulate_mouse_down(target, button, Modifiers::none());
        cx.simulate_mouse_up(target, button, Modifiers::none());
        settle(cx);

        assert_eq!(
            cx.opened_url(),
            None,
            "{button:?} on the link text should not open the URL"
        );
    }
}

/// 按下与抬起必须是同一个左键：左键按下、右键抬起不算点击。
#[gpui_kit::test]
fn releasing_the_right_button_after_pressing_the_left_should_open_nothing(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, LINK_TEXT, None);
    let link = inside_block(bounds(cx, BLOCK_0));

    cx.simulate_mouse_move(link, None::<MouseButton>, Modifiers::none());
    cx.simulate_mouse_down(link, MouseButton::Left, Modifiers::none());
    assert!(
        pressed_link(cx, &view).is_some(),
        "the left button went down on the link text, so a pressed link should be recorded"
    );

    cx.simulate_mouse_up(link, MouseButton::Right, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        None,
        "releasing the right button should not complete a left-button press on the link"
    );
}

/// 右键按下、左键抬起也不算点击：右键不该给一次左键点击留下可用的按下点。
#[gpui_kit::test]
fn releasing_the_left_button_after_pressing_the_right_should_open_nothing(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, LINK_TEXT, None);
    let link = inside_block(bounds(cx, BLOCK_0));

    cx.simulate_mouse_move(link, None::<MouseButton>, Modifiers::none());
    cx.simulate_mouse_down(link, MouseButton::Right, Modifiers::none());
    assert_eq!(
        pressed_link(cx, &view),
        None,
        "a right-button press on the link should not record a pressed link"
    );

    cx.simulate_mouse_up(link, MouseButton::Left, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        None,
        "a right-button press should not be completed by releasing the left button on the link"
    );
}

/// 在链接之外按下、拖到链接上再松手不算点击：按下点必须落在同一条链接上。
#[gpui_kit::test]
fn releasing_on_a_link_after_pressing_elsewhere_should_open_nothing(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, LINK_TEXT, None);
    let block = bounds(cx, BLOCK_0);
    // 块内、链接文字右侧的空白（链接文字只占块左边一小段）。
    let blank = block.origin + point(block.size.width - px(2.), px(2.));
    let link = inside_block(block);

    cx.simulate_mouse_down(blank, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(link, None::<MouseButton>, Modifiers::none());
    cx.simulate_mouse_up(link, MouseButton::Left, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        None,
        "releasing on the link after pressing outside it should not open the URL"
    );
}

/// 上一条的正对照（`opened_url()` 不重置，所以单独一条测试）：同一条链接上按下并抬起会打开。
#[gpui_kit::test]
fn pressing_and_releasing_on_the_same_link_should_open_it(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, LINK_TEXT, None);
    let link = inside_block(bounds(cx, BLOCK_0));

    cx.simulate_mouse_down(link, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(link, MouseButton::Left, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        Some(String::from("https://example.com")),
        "pressing and releasing on the link itself should open the URL it points at"
    );
}

/// 同一份 `MarkdownState` 被两个 `Markdown` 实例渲染时，点击第一个实例里的链接仍能打开。
#[gpui_kit::test]
fn clicking_a_link_in_the_first_copy_of_a_shared_state_should_open_it(cx: &mut TestAppContext) {
    let (_view, cx) = two_copies(cx, LINK_TEXT);

    let link = inside_block(bounds(cx, FIRST_COPY));
    cx.simulate_click(link, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        Some(String::from("https://example.com")),
        "the first copy should open the URL of the link it draws"
    );
}

/// 同一份 `MarkdownState` 被两个 `Markdown` 实例渲染时，点击第二个实例里的链接同样能打开。
#[gpui_kit::test]
fn clicking_a_link_in_the_second_copy_of_a_shared_state_should_open_it(cx: &mut TestAppContext) {
    let (_view, cx) = two_copies(cx, LINK_TEXT);

    let link = inside_block(bounds(cx, SECOND_COPY));
    cx.simulate_click(link, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        Some(String::from("https://example.com")),
        "the second copy should open the URL of the link it draws"
    );
}

/// 指针移到链接上标记 hover，移出块后归零。
#[gpui_kit::test]
fn moving_the_pointer_on_and_off_a_link_should_toggle_the_hovered_link(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, LINK_TEXT, None);
    let block = bounds(cx, BLOCK_0);

    assert_eq!(
        hovered_link(cx, &view),
        None,
        "nothing has been hovered yet, so the document should start with no hovered link"
    );

    cx.simulate_mouse_move(inside_block(block), None::<MouseButton>, Modifiers::none());
    assert!(
        hovered_link(cx, &view).is_some(),
        "the pointer sits on the link text, so the document should report a hovered link"
    );

    // 块正下方 20px，仍在窗口内、但在块之外。
    let below_block = block.origin + point(px(2.), block.size.height + px(20.));
    cx.simulate_mouse_move(below_block, None::<MouseButton>, Modifiers::none());
    assert_eq!(
        hovered_link(cx, &view),
        None,
        "the pointer left the block, so nothing should stay hovered"
    );
}

/// `set_source` 换文档后：hover 归零、旧文档多出来的块不再绘制、`source()` 是新原文。
#[gpui_kit::test]
fn setting_a_new_source_should_reset_hover_and_drop_the_old_blocks(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "# Title\n\n[site](https://example.com)", None);
    // 旧文档有两个块，第二个是那段链接；`bounds` 会先重画。
    let link_block = bounds(cx, BLOCK_1);

    cx.simulate_mouse_move(
        inside_block(link_block),
        None::<MouseButton>,
        Modifiers::none(),
    );
    assert!(
        hovered_link(cx, &view).is_some(),
        "the pointer sits on the second block's link, so it should be hovered"
    );

    set_source(cx, &view, "only paragraph");

    assert_eq!(
        hovered_link(cx, &view),
        None,
        "a new document should not inherit the old one's hovered link"
    );
    assert!(
        cx.debug_bounds(BLOCK_0).is_some(),
        "the new document's single block should still be drawn"
    );
    assert!(
        cx.debug_bounds(BLOCK_1).is_none(),
        "the new document has one block, so the old second block should be gone"
    );
    assert_eq!(
        cx.read(|app| view.read(app).state.read(app).source().to_string()),
        "only paragraph",
        "`source()` should report the source the state was last set to"
    );
}

/// `set_source` 把按下状态一起清掉：新文档里的链接不该继承旧文档留下的按下点。
///
/// 两份文档都只有一条链接（各自 `links` 表里的第 0 条），所以旧按下点残留时 `LinkId`
/// 恰好与新文档里那条链接相等，在新链接上抬手会被当成"同一条链接上的点击"而打开。
#[gpui_kit::test]
fn setting_a_new_source_should_clear_the_pressed_link(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, LINK_TEXT, None);
    let link = inside_block(bounds(cx, BLOCK_0));

    cx.simulate_mouse_down(link, MouseButton::Left, Modifiers::none());
    assert!(
        pressed_link(cx, &view).is_some(),
        "pressing the link text should record a pressed link"
    );

    set_source(cx, &view, "[other](https://other.com)");

    assert_eq!(
        pressed_link(cx, &view),
        None,
        "a new document should not inherit the old one's pressed link"
    );

    // 在新文档那条链接上抬手：这次手势的按下点已经没了，所以不算点击。
    let other = inside_block(bounds(cx, BLOCK_0));
    cx.simulate_mouse_up(other, MouseButton::Left, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        None,
        "releasing on the new document's link should not open it: its press is gone"
    );
}

/// 攻击者序列：同一份 `MarkdownState` 渲染两遍（文档里只有一条链接 `https://e.com/1`）。
///
/// 1. 在**副本 1** 的链接上按下；
/// 2. 移到两个块**之外**抬起（这次手势没有落到任何块，不打开任何东西，但会留下按下记录）；
/// 3. 移到**副本 2** 的链接上**只抬起**（没有新的按下）；
/// 4. 不得打开 `https://e.com/1`。
///
/// `LinkId` 是文档级的：两个副本画的是同一条链接、取到的是同一个 id，所以"按下点与松开点
/// 在同一条链接上"这条判据本身拦不住跨实例的误开（第 3 步的 `link_at` 恰好也是 `LinkId(0)`）。
/// 拦得住的是按下记录里的**位置**：第 1 步的按下点落在副本 1 的 `bounds` 内，副本 2 在
/// `MouseUp` 里要求 `bounds.contains(&pressed.at)`，于是这次抬手不是副本 2 的点击。
#[gpui_kit::test]
fn releasing_on_the_second_copy_after_pressing_the_first_should_open_nothing(
    cx: &mut TestAppContext,
) {
    let (_view, cx) = two_copies(cx, "[one](https://e.com/1)");

    let first_link = inside_block(bounds(cx, FIRST_COPY));
    cx.simulate_mouse_down(first_link, MouseButton::Left, Modifiers::none());

    // 两个块之外的坐标：副本 2 的块下方 20px，仍在窗口内。
    let second_copy = bounds(cx, SECOND_COPY);
    let outside = second_copy.origin + point(px(2.), second_copy.size.height + px(20.));
    cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::none());
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        None,
        "releasing outside both blocks should not open anything"
    );

    // 没有新的 MouseDown：这次抬手不属于副本 2 的任何一次手势。
    let second_link = inside_block(second_copy);
    cx.simulate_mouse_up(second_link, MouseButton::Left, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url(),
        None,
        "a release without a press inside this block must not open the link"
    );
}

/// hover 从"没有链接"变成"命中链接"时必须请求重绘，否则下划线要等下一次无关重绘才出现。
#[gpui_kit::test]
fn hovering_a_link_should_request_a_redraw(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, LINK_TEXT, None);
    let state = cx.read(|app| view.read(app).state.clone());
    let notifications = Rc::new(Cell::new(0));
    let counter = notifications.clone();
    // 订阅体要活到断言之后：`Subscription` 一被释放，实体上的观察者就跟着注销。
    let _observation = cx.update(|_, app| {
        app.observe(&state, move |_, _| {
            counter.set(counter.get() + 1);
        })
    });

    let target = inside_block(bounds(cx, BLOCK_0));
    let before = notifications.get();

    cx.simulate_mouse_move(target, None::<MouseButton>, Modifiers::none());
    settle(cx);

    let after = notifications.get();
    assert!(
        after > before,
        "moving the pointer onto the link should notify the state entity so the underline gets \
         drawn: {after} should exceed {before}"
    );
}

/// kbd 内容不参与折行：整段比 [`NARROW`] 还宽也不拆行，与同样文字的普通段落相比块高更小。
///
/// 两个 kbd 版本（长、短）都是一行高，所以块高相等：长的那份如果被折了行，高度会向普通
/// 段落看齐（普通段落就是同一段文字的对照）。
#[gpui_kit::test]
fn kbd_content_should_not_wrap(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, &format!("<kbd>{LONG_TEXT}</kbd>"), Some(NARROW));
    let long_kbd = bounds(cx, BLOCK_0);

    set_source(cx, &view, &format!("<kbd>{SHORT_TEXT}</kbd>"));
    let short_kbd = bounds(cx, BLOCK_0);

    set_source(cx, &view, LONG_TEXT);
    let plain = bounds(cx, BLOCK_0);

    assert_eq!(
        long_kbd.size.height, short_kbd.size.height,
        "a kbd run is one line tall no matter how long its content is"
    );
    assert!(
        plain.size.height > long_kbd.size.height,
        "the same text outside kbd wraps onto more lines: {} should exceed {}",
        plain.size.height,
        long_kbd.size.height
    );
}

/// kbd 里的显式换行仍然换行：跨行的 kbd 内容按行拆成两个框，块高严格变大。
#[gpui_kit::test]
fn a_line_break_inside_kbd_should_split_the_box_across_lines(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "<kbd>first line\nsecond line</kbd>", None);
    let two_lines = bounds(cx, BLOCK_0);

    set_source(cx, &view, "<kbd>first linesecond line</kbd>");
    let one_line = bounds(cx, BLOCK_0);

    assert!(
        two_lines.size.height > one_line.size.height,
        "the soft break inside kbd must still break the line: {} should exceed {}",
        two_lines.size.height,
        one_line.size.height
    );
}

/// kbd 里可以同时有图片与文字，两者仍然是一段不拆开的内容：图片不会独占一行。
///
/// 这是占位文案的形态（`common.market_not_loaded` 把控件写成
/// `<kbd>![](file://icons/phosphoricons/graph.svg) 市场</kbd>`）：图片与它旁边的字要一起走。
/// 段内不能拆分，所以图片多占的宽度只会让整段更早换到下一行，不会把图片挤到单独一行——
/// 那会让块高出一行。
///
/// 文案必须真的长到会折行，否则两种写法都是一行，断言等于什么都没测（短文案实测两份都是
/// 204×26，图片那段只占约三分之一宽，没有换行机会）。
///
/// 只比高度，不比宽度：块宽恒等于容器宽（`TextFlow` 自报折行宽度），
/// 图片再宽也不会让块变宽。
#[gpui_kit::test]
fn an_image_inside_kbd_should_not_get_a_line_of_its_own(cx: &mut TestAppContext) {
    let (view, cx) = harness(
        cx,
        &format!("<kbd>{GRAPH_IMAGE} {LONG_TEXT}</kbd>"),
        Some(NARROW),
    );
    let with_image = bounds(cx, BLOCK_0);

    set_source(cx, &view, &format!("<kbd>{LONG_TEXT}</kbd>"));
    let without_image = bounds(cx, BLOCK_0);

    set_source(cx, &view, LONG_TEXT);
    let plain = bounds(cx, BLOCK_0);

    assert_eq!(
        with_image.size.height, without_image.size.height,
        "the image stays inside the run it belongs to rather than taking a line of its own: {} \
         should equal {}",
        with_image.size.height, without_image.size.height
    );
    assert!(
        plain.size.height > with_image.size.height,
        "and the run still refuses to break, unlike the same text outside kbd: {} should exceed {}",
        plain.size.height,
        with_image.size.height
    );
}

/// kbd 框的高度认图片：框里那张 40px 的图比行高还高，框与整行都跟着它长高。
///
/// 段内内容的纵向尺寸是"文字字形高与图片高的较大者"，这条就钉住图片那一半：若框高只按文字算，
/// 40px 的那份会与 16px 的那份一样高。
#[gpui_kit::test]
fn an_image_inside_kbd_should_stretch_its_box(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, &format!("<kbd>x{TALL_IMAGE}y</kbd>"), Some(NARROW));
    let with_tall_image = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, &format!("<kbd>x{IMAGE}y</kbd>"));
    let with_short_image = bounds(cx, BLOCK_0).size.height;

    assert!(
        with_tall_image > with_short_image,
        "a 40px image inside kbd should stretch the box that holds it: {with_tall_image:?} should \
         exceed {with_short_image:?}"
    );
}

/// 本帧画出来的 kbd 框的外接矩形（缩放后的像素；两个数只用来互相比较，不与 `Pixels` 混用）。
///
/// 框是 `paint_quad` 画的：`debug_bounds` 只答块的位置，框自己那个矩形只有这里读得到。
/// 一帧的 quad 里只有它有"**透明的**纯色底"——本组件唯一一处 `outline` 画法（见 `inline_flow`
/// 里 kbd 框那一段，链接下划线走的是 `fill`），所以这就是框的判别式：不必再按块几何去猜哪些
/// quad 属于这个框，也不会把别的块或别的元素的 quad 收进来。
///
/// **框记的不是一个 quad**：gpui 把"只有边框、底透明"的 quad 拆成上 / 下 / 左 / 右四条带
/// （`Window::paint_quad` 里 `largest_border_interior` 那一段），每条各自记一条 primitive，四条
/// 的 `bounds` 都是整个框的矩形，各自真正画到的那一条只在 `content_mask` 里。本仓库实测两者
/// 给出的宽高相同，但口径写在契约上的是 `content_mask`（"quads are not clipped"），所以这里按
/// 它合并出外接矩形。
///
/// 返回的是**整个窗口**里那唯一的框：这几条用例都只画一个 kbd 框。
fn kbd_box_size(cx: &mut VisualTestContext) -> (f32, f32) {
    let strips: Vec<_> = cx
        .update(|window, _| window.painted_quads())
        .into_iter()
        .filter(|quad| {
            quad.background
                .as_solid()
                .is_some_and(|color| color.is_transparent())
        })
        .collect();
    let Some((first, rest)) = strips.split_first() else {
        panic!("the kbd box should be painted as a border-only quad");
    };

    let painted = rest
        .iter()
        .chain([first])
        .map(|strip| strip.content_mask.bounds)
        .reduce(|a, b| {
            Bounds::from_corners(
                point(a.origin.x.min(b.origin.x), a.origin.y.min(b.origin.y)),
                point(a.right().max(b.right()), a.bottom().max(b.bottom())),
            )
        })
        .unwrap_or(first.content_mask.bounds);

    (painted.size.width.0, painted.size.height.0)
}

/// kbd 框画出来的矩形两边各认图片的一边：同一段文字里换成那张 `size=16,64` 的图，框要比放
/// 16×16 那张时**更高**（高来自图片的高），而**一样宽**（宽来自图片的宽）。
///
/// 这条钉的是框的**绘制几何**（[`kbd_box_size`] 读的是画出来的那个矩形），与
/// [`an_image_inside_kbd_should_be_padded_around_its_own_height`] 钉的块高是两个面：框在行里
/// 垂直居中，行高被图片自己撑起来时块高会把框的高度盖住——所以"框多高"必须有这一条才读得到。
#[gpui_kit::test]
fn an_image_inside_kbd_should_be_framed_by_its_own_width_and_height(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, &format!("<kbd>x{RECTANGLE_IMAGE}y</kbd>"), Some(NARROW));
    let (rectangle_width, rectangle_height) = kbd_box_size(cx);

    set_source(cx, &view, &format!("<kbd>x{IMAGE}y</kbd>"));
    let (square_width, square_height) = kbd_box_size(cx);

    assert!(
        rectangle_height > square_height,
        "the box should be as tall as the image it frames: {rectangle_height:?} should exceed \
         {square_height:?}"
    );
    assert_eq!(
        rectangle_width, square_width,
        "both images are 16 wide, so both boxes should be the same width"
    );
}

/// kbd 框的上下内边距是围着图片自己的**高**留的：同一张 16×64 的图，套进 `<kbd>` 之后那一行
/// 比它单独占一行还高，高出的正是框的上下内边距。
///
/// 框内高度若按图片宽度（16）算，框高就成了 16 + 2×内边距、比图片本身还矮，行高于是与不套框时
/// 相等，这条断言会失败。
#[gpui_kit::test]
fn an_image_inside_kbd_should_be_padded_around_its_own_height(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, &format!("<kbd>x{RECTANGLE_IMAGE}y</kbd>"), Some(NARROW));
    let in_a_box = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, &format!("x{RECTANGLE_IMAGE}y"));
    let plain = bounds(cx, BLOCK_0).size.height;

    assert!(
        in_a_box > plain,
        "the kbd box pads the image's own height, so its line should be taller than the same image \
         without the box: {in_a_box:?} should exceed {plain:?}"
    );
}

/// kbd 段的宽度也认图片：段里那张 200px 的图让整段宽过容器，整段（不是图）移到下一行，
/// 块因此多出一行。若图片的宽度不计入 kbd 段，段宽不变、这一行不会多出来。
#[gpui_kit::test]
fn a_wide_image_inside_kbd_should_count_towards_the_run_width(cx: &mut TestAppContext) {
    let (view, cx) = harness(
        cx,
        &format!("short <kbd>xx{WIDE_KBD_IMAGE}</kbd>"),
        Some(NARROW),
    );
    let with_wide_image = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, &format!("short <kbd>xx{GRAPH_IMAGE}</kbd>"));
    let with_narrow_image = bounds(cx, BLOCK_0).size.height;

    assert!(
        with_wide_image > with_narrow_image,
        "a 200px image should push the whole run past the first line: {with_wide_image:?} should \
         exceed {with_narrow_image:?}"
    );
}

/// kbd 与链接可以叠加：框里的链接文字照旧可点，按下的位置要越过框的内边距。
#[gpui_kit::test]
fn a_link_inside_kbd_should_still_open(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, "<kbd>[kbd link](https://example.com)</kbd>", None);
    let block = bounds(cx, BLOCK_0);
    // 框在文字左侧留了内边距，块左上角是框的边缘；水平方向越过内边距、垂直方向取行中心。
    let target = block.origin + point(px(12.), block.size.height / 2.);

    cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com"),
        "a link inside a kbd run should open like any other link"
    );
}

/// 对齐只动 x，不动折行：同一段长文字在左 / 居中 / 右对齐下折成一样多的行（块高相同）。
///
/// 这条只守"折行结果不变"：它比的是块高，所以"偏移量算错"这类缺陷它看不出来——那种缺陷由
/// [`a_wrapped_aligned_paragraph_should_align_its_last_line`] 与几条点击用例守住。
#[gpui_kit::test]
fn alignment_should_not_change_how_a_paragraph_wraps(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, LONG_TEXT, Some(NARROW));
    let left = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, &format!("<p align=\"center\">{LONG_TEXT}</p>"));
    let center = bounds(cx, BLOCK_0).size.height;

    set_source(cx, &view, &format!("<p align=\"right\">{LONG_TEXT}</p>"));
    let right = bounds(cx, BLOCK_0).size.height;

    assert_eq!(
        left, center,
        "centring a paragraph should not change how many lines it wraps onto"
    );
    assert_eq!(
        center, right,
        "right aligning a paragraph should not change how many lines it wraps onto"
    );
}

/// 折行后的每一行各自对齐：右对齐的长段落，链接所在的最后一行贴着块右边缘，那里点得中。
///
/// 若对齐只对整段生效（或只对单行段落生效），最后一行会落在左边、这一点就点不中。
#[gpui_kit::test]
fn a_wrapped_aligned_paragraph_should_align_its_last_line(cx: &mut TestAppContext) {
    let (_view, cx) = harness(
        cx,
        &format!("<p align=\"right\">{LONG_TEXT} {LINK_TEXT}</p>"),
        Some(NARROW),
    );
    let block = bounds(cx, BLOCK_0);
    // 最后一行贴着块底边：在它右端 2px 处按下并抬起。
    let target = block.origin + point(block.size.width - px(2.), block.size.height - px(2.));

    cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url().as_deref(),
        Some(LINK_URL),
        "the last line of a right aligned paragraph should end at the block's right edge"
    );
}

/// 整段只有一张图片时，图片也是可见内容：右对齐会把它推到右边缘，链接命中盒跟着走。
#[gpui_kit::test]
fn an_aligned_paragraph_of_one_image_should_move_that_image(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, &format!("<p align=\"right\">{LINKED_IMAGE}</p>"), None);
    let block = bounds(cx, BLOCK_0);

    cx.simulate_click(inside_block(block), Modifiers::none());
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        None,
        "the image should have left the block's left edge"
    );

    cx.simulate_click(
        block.origin + point(block.size.width - px(2.), block.size.height / 2.),
        Modifiers::none(),
    );
    settle(cx);
    assert_eq!(
        cx.opened_url().as_deref(),
        Some(LINK_URL),
        "a single image is visible content, so right alignment should push it to the right edge"
    );
}

/// 换文档之后对齐跟着新文档走：同一份状态从居中文档换成普通段落，左边缘重新变回链接。
///
/// 用 hover 而不是点击来判：`opened_url()` 不会重置，点过一次之后就没法再断言"点不到"。
#[gpui_kit::test]
fn changing_the_source_should_change_where_the_text_sits(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, &format!("<p align=\"center\">{LINK_TEXT}</p>"), None);
    let block = bounds(cx, BLOCK_0);

    cx.simulate_mouse_move(inside_block(block), None::<MouseButton>, Modifiers::none());
    settle(cx);
    assert_eq!(
        hovered_link(cx, &view),
        None,
        "the centered link should not sit at the block's left edge"
    );

    set_source(cx, &view, LINK_TEXT);

    let block = bounds(cx, BLOCK_0);
    cx.simulate_mouse_move(inside_block(block), None::<MouseButton>, Modifiers::none());
    settle(cx);
    assert!(
        hovered_link(cx, &view).is_some(),
        "a plain paragraph should put its link back at the block's left edge"
    );
}

/// 对齐偏移的算式：左对齐恒 0、居中取一半、右对齐顶到右边，内容比行盒还宽时一律不偏移
/// （溢出留在右侧）。整行没有可见内容时由 `align_lines` 自己跳过，不走这里。
#[test]
fn align_offset_should_place_a_line_inside_its_box() {
    use super::{document::ParagraphAlign, inline_flow::align_offset};

    assert_eq!(
        align_offset(ParagraphAlign::Left, px(100.), px(40.)),
        px(0.)
    );
    assert_eq!(
        align_offset(ParagraphAlign::Right, px(100.), px(40.)),
        px(60.)
    );
    assert_eq!(
        align_offset(ParagraphAlign::Center, px(100.), px(40.)),
        px(30.)
    );
    assert_eq!(
        align_offset(ParagraphAlign::Right, px(100.), px(400.)),
        px(0.),
        "content wider than its box overflows to the right rather than shifting left"
    );
    assert_eq!(
        align_offset(ParagraphAlign::Center, px(100.), px(400.)),
        px(0.)
    );
}

/// 显式写 `align="left"` 与普通的左对齐段落没有区别：文字仍在左边缘，块左上角就能点到链接。
///
/// 这是"左对齐偏移恒为 0"那条不变量的端到端对照：偏移若按居中 / 右对齐算，链接会离开左边缘。
#[gpui_kit::test]
fn an_explicit_left_alignment_should_keep_its_text_at_the_left_edge(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, &format!("<p align=\"left\">{LINK_TEXT}</p>"), None);
    let block = bounds(cx, BLOCK_0);

    cx.simulate_click(inside_block(block), Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url().as_deref(),
        Some(LINK_URL),
        "an explicit left alignment should leave the text where a plain paragraph puts it"
    );
}

/// 居中把整行推到行盒中间：链接文字的命中盒跟着走，块左边缘不再是链接，块的水平中心才是。
///
/// 先点左边缘（什么都没打开）再点中心（打开了），所以不需要 `opened_url()` 重置。
#[gpui_kit::test]
fn a_centered_paragraph_should_push_its_text_to_the_middle(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, &format!("<p align=\"center\">{LINK_TEXT}</p>"), None);
    let block = bounds(cx, BLOCK_0);

    cx.simulate_click(
        block.origin + point(px(2.), block.size.height / 2.),
        Modifiers::none(),
    );
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        None,
        "the centered link text should have left the block's left edge"
    );

    cx.simulate_click(
        block.origin + point(block.size.width / 2., block.size.height / 2.),
        Modifiers::none(),
    );
    settle(cx);
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com"),
        "the centered link text should sit at the block's horizontal center"
    );
}

/// 右对齐把整行推到行盒右边缘：链接文字的命中盒贴着右边缘，块左边缘不再是链接。
#[gpui_kit::test]
fn a_right_aligned_paragraph_should_push_its_text_to_the_right_edge(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, &format!("<p align=\"right\">{LINK_TEXT}</p>"), None);
    let block = bounds(cx, BLOCK_0);

    cx.simulate_click(
        block.origin + point(px(2.), block.size.height / 2.),
        Modifiers::none(),
    );
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        None,
        "the right aligned link text should have left the block's left edge"
    );

    cx.simulate_click(
        block.origin + point(block.size.width - px(2.), block.size.height / 2.),
        Modifiers::none(),
    );
    settle(cx);
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com"),
        "right aligned text should end at the block's right edge"
    );
}

/// kbd 框跟着整行一起偏移：右对齐时框与框里的链接都贴到右边缘，而框内仍留着右侧内边距。
#[gpui_kit::test]
fn a_right_aligned_kbd_run_should_move_with_its_line(cx: &mut TestAppContext) {
    let (_view, cx) = harness(
        cx,
        &format!("<p align=\"right\"><kbd>{LINK_TEXT}</kbd></p>"),
        None,
    );
    let block = bounds(cx, BLOCK_0);
    // 框的右边缘就是行盒右边缘，链接文字在框的右侧内边距之内，所以从右边退 12px 命中文字。
    let target = block.origin + point(block.size.width - px(12.), block.size.height / 2.);

    cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    settle(cx);

    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com"),
        "the kbd box should move with the line it belongs to, taking its link along"
    );
}

/// 位图一律按**内嵌资源**交给 `img()`，绝不按字符串构造：`ImageSource` 的 `From<&str>` /
/// `From<SharedString>` 会拿 `url::Url` 问一次，能被当成 URL 的路径会变成 `Resource::Uri`，
/// 而它的加载器真去发网络请求（gpui 的 `ImageAssetLoader::load`）。资源路径只该有"问资源源"
/// 一种解释。
///
/// # 这条测试守得住什么、守不住什么
///
/// `img()` 画出来的元素没有调试选择器，`TestAppContext` 又不给装资源源（写死空源，也没有
/// setter），所以"元素最后拿到的是哪个来源"在测试里根本看不见。能钉住的只有**构造**这一环：
/// 把 `embedded_image_source` 的函数体改回 `path.into()` 这条测试就会失败。
///
/// 反过来，只把调用点改回 `img(path.clone())`、把这个函数留在原地不动，这条测试会**通过**
/// ——挡下那种改法的是编译器：`embedded_image_source` 从此没人调用，`cargo clippy -D warnings`
/// 会以 `dead_code` 报错。两者合起来才是完整的守卫：测试钉住"构造出的是什么"，lint 钉住
/// "构造还得被用上"。
#[test]
fn a_bitmap_image_should_be_an_embedded_resource_not_a_uri() {
    for path in [
        "brands/Binance/binance.png",
        "http://evil.example/x.png",
        "file://brands/Binance/binance.png",
        "C:/Windows/win.ini",
    ] {
        match super::inline_flow::embedded_image_source(SharedString::from(path)) {
            ImageSource::Resource(Resource::Embedded(embedded)) => assert_eq!(
                embedded.as_ref(),
                path,
                "the asset path should reach the asset source verbatim"
            ),
            ImageSource::Resource(_) => {
                panic!("a bitmap should be an embedded resource, not a URI")
            }
            _ => panic!("a bitmap should be a resource"),
        }
    }

    // 另一批路径按字符串构造时会变成 URI——这正是上面那条构造必须显式写资源的原因，
    // 也是这条测试存在的理由。
    for path in [
        "http://evil.example/x.png",
        "file://brands/Binance/binance.png",
        "C:/Windows/win.ini",
    ] {
        assert!(
            matches!(
                ImageSource::from(SharedString::from(path)),
                ImageSource::Resource(Resource::Uri(_))
            ),
            "`{path}` should be the shape `From<SharedString>` mistakes for a URI"
        );
    }
}

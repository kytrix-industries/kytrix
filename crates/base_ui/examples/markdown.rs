mod support;

use base_ui::{
    assets::Assets,
    button::Button,
    markdown::view::{Markdown, MarkdownState},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions, div, px,
    size,
};
use smol_str::format_smolstr;

/// The document the window opens on.
///
/// It puts one of every visible shape on screen at once: six heading levels, a paragraph carrying
/// all four inline styles, a paragraph of `<kbd>` keys with the theme's border drawn around each
/// one (including a run that overflows instead of wrapping), inline images of several sizes
/// flowing with the words around them (`img` bitmaps under `assets/brands` and `svg` icons under
/// `assets/icons`), a paragraph long enough that its image has to take part in the wrapping, the
/// three paragraph alignments a `<p align="…">` tag pair can ask for, the blank lines the source
/// writes between blocks (one line each, however many were written), and one block each of the
/// shapes the component drops whole, so that "nothing is drawn" is as visible here as what is
/// drawn.
///
/// The alignment tag pair has a narrow contract, which the paragraph before those three lines
/// spells out for the reader: the open tag has to be an element of a top-level HTML block (not
/// inside a list, a quote or an indented block), it closes at the first `</p>` — blank lines in
/// between belong to the pair, so a pair may run across them — and everything in that HTML block
/// other than the tag pairs themselves is dropped. So a plain sentence written directly under a
/// tag pair disappears, and so does a tag pair whose attribute is not exactly `left`, `right` or
/// `center`. A missing attribute means `left`. What a pair spans is drawn as it stands: its
/// paragraphs take the pair's alignment, and a heading inside it stays a heading.
const DOCUMENT: &str = r#"# Heading level one

One paragraph carries every inline style at once: plain words, **bold words**, *italic words* and a [link](https://example.com) beside 同一行里的中文。

The three lines below ask for their own alignment with a p tag pair: the open tag is an element of a top-level HTML block, and it carries at most one attribute, align, whose value is left, right or center. 这一块里除标签对以外的内容一个字都不会显示，所以下面那三行各自成段，中间不留空行也照样分得开；标签对本身可以跨空行，跨过去的内容照常画出来——段落带这一对的对齐，标题照旧是标题。

<p align="center">这一行居中：centred by the tag pair.</p>
<p align="right">这一行右对齐：right aligned by the tag pair.</p>
<p>没有 align 属性，按默认的左对齐：left by default.</p>

<p align="center">这一对跨了一个空行，所以它下面先是一行空行：

# 被跨过去的标题照旧是标题

而这一段又回到这一对的对齐。</p>

Keys are wrapped in a rounded border: press <kbd>Ctrl</kbd> + <kbd>C</kbd> to copy, <kbd>Ctrl</kbd> + <kbd>V</kbd> to paste, and <kbd>**Esc**</kbd> or the <kbd>[Enter](https://example.com/keybindings)</kbd> inside this sentence keeps 它的行内样式。A kbd run never wraps: <kbd>this-single-run-is-not-broken-onto-a-second-line-even-when-it-overflows-the-container-width</kbd> stays on one line. 显式换行仍然换行，所以这一对标签会按行画两个框：<kbd>first
second</kbd>。

Inline images flow with the words around them: an svg icon ![a](file://icons/phosphoricons/arrow-up.svg), the same icon at 24 pixels ![a](file://icons/phosphoricons/arrow-up.svg?size=24), a bitmap at its own 128×128 ![a](file://brands/Binance/binance.png), the same bitmap squeezed into 32×32 ![a](file://brands/Binance/binance.png?size=32), and one whose two edges differ ![a](file://brands/Solana/colored@72px.png?size=24,48) all sit on the same line as these words.

The brand bitmaps keep their own shape when no size is asked for: ![a](file://brands/Ethereum/colored@72px.png) is 44×72 in the asset itself, ![a](file://brands/Hyperliquid/hyperliquid.png?size=40) is 180×180 there and is drawn at 40, and ![a](file://brands/OKX/okx.png?size=24) needs its size written out to stop being 200×200.

This paragraph is deliberately longer than one line, so the image in the middle of it has to take part in the wrapping instead of being pushed onto a line of its own: 当一段文字很长的时候，它会按照可用宽度自动折行，而 ![a](file://icons/phosphoricons/graph.svg?size=20) 这个图标必须和两侧的中文、English words 一起折行，既不能把整段拆成三行，也不能独自占一行。The sentence keeps going with enough English words to make the line break land in several different places, including somewhere after the image.

A 40-pixel image makes its own line taller than the rest: ![a](file://icons/phosphoricons/arrow-up.svg?size=40) the words beside it stay on that line, centred against the image.

An unknown type ![a](file://icons/phosphoricons/arrow-up.svg?type=ico) and an image that is not a file URI at all ![a](https://example.com/a.png) are ignored whole, alt text included, so this sentence closes the gap they leave; an asset that is simply not there ![a](file://nope/nothing.png) keeps the 16-pixel box the fallback size gives it, so a hole stays where the words around it part.

soft break line one
soft break line two

A blank line in the source is a blank line on screen: this paragraph and the one above it are held apart by exactly the one blank line written between them.



These three blank lines are drawn as three, not collapsed into one. A paragraph written right under another block with no blank line between them keeps the minimum gap instead.

Whitespace is never collapsed here: the six spaces between these two words      take up their width and can make the line wrap, unlike in a browser.

The next three blocks are the shapes the component drops with their whole subtree ![a](file://icons/phosphoricons/warning-circle.svg?size=18), so the list, the quote and the fenced code block below each draw not one character:

- list item that must not be drawn

> quote that must not be drawn

```rust
fn code_that_must_not_be_drawn() -> &'static str {
    "not one character of this block is drawn"
}
```

## Heading level two

### Heading level three

#### Heading level four

##### Heading level five

###### Heading level six

中文与 English 混排：这一段用来观察 CJK 折行，标点、空格与 English words 交替出现，长度足以折成好几行，而两张图片 ![a](file://brands/Binance/binance.png?size=20) 和 ![a](file://brands/OKX/okx.png?size=22) 跟着文字一起走。
"#;

/// The document the toggle button switches to.
///
/// It is deliberately a different shape: one first-level heading, three short paragraphs and one
/// link, so pressing the button changes both the number of blocks and the length the status line
/// reports while the state entity itself stays the same.
const ALTERNATE: &str = r#"# 第二份文档

第二份 markdown 短得多，切换之后块的数量与状态行里的字节数都会立刻变化。

第二段用来看块与块之间的间距。

A [second link](https://example.com/second) is the only thing in this document that can be clicked.
"#;

/// The example view: one markdown state entity, two documents to hand it, and the button that
/// swaps them.
struct MarkdownExample {
    /// What the component renders. This view never parses anything itself; it replaces the source
    /// and lets the state entity parse it once.
    state: Entity<MarkdownState>,
    /// Which of the two documents is on screen, and the only thing the button changes.
    alternate: bool,
}

impl MarkdownExample {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            state: cx.new(|cx| MarkdownState::new(DOCUMENT, cx)),
            alternate: false,
        }
    }
}

impl Render for MarkdownExample {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let index = if self.alternate { 2 } else { 1 };
        let source_len = self.state.read(cx).source().len();

        div()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            // The markdown scrolls; the button and the status line stay put below it.
            .child(
                div()
                    .id("markdown-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_6()
                    .text_color(theme.roles.fg.default.hsla)
                    .child(Markdown::new(&self.state)),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_6()
                    .pt_4()
                    .pb_6()
                    .border_t_1()
                    .border_color(theme.roles.line.divider.hsla)
                    .child(
                        Button::new("toggle-document")
                            .child(SharedString::from("切换文档"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.alternate = !this.alternate;
                                let next = if this.alternate { ALTERNATE } else { DOCUMENT };
                                this.state
                                    .update(cx, |state, cx| state.set_source(next, cx));
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.roles.fg.default.hsla)
                            .child(SharedString::new(format_smolstr!(
                                "当前文档第 {index} 份 · {source_len} 字节"
                            ))),
                    ),
            )
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(760.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(MarkdownExample::new),
        )
        .expect("failed to open window");
    });
}

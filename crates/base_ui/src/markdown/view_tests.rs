//! `MarkdownState` 的公开面测试。
//!
//! 状态实体的字段是私有的，也不能放宽可见性，所以这里只断言公开方法
//! （`new` / `set_source` / `source`）与渲染结果（调试选择器 + `debug_bounds`）。
//!
//! hover 用例不在这里重复：`markdown::inline_flow_tests` 已经有一条完整的
//! 鼠标事件序列（移入链接后 `hovered_link()` 为 `Some`，移出块后归零），在这里再写一遍
//! 只是把同一段事件序列和同一个断言维护两次，不会覆盖新的行为。

use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled as _,
    TestAppContext, VisualTestContext, Window, div,
};
use i18n::{global::embedded, language::codes::LanguageCode, t};

use super::{
    document::LinkId,
    view::{Markdown, MarkdownState},
};
use crate::style;

/// 两个块，用来验证"块数变少"。
const TWO_BLOCKS: &str = "# Title\n\ntext";
/// 只有一个块，用来验证块数确实跟着新文档变。
const ONE_BLOCK: &str = "# Title";

/// 一条居中标签对的闭标签。文案里的标签对配没配成，按它判定。
const ALIGN_TAG_CLOSE: &str = "</p>";

/// alert 的文案：对话框里的句子与拒绝语画的都是 `Markdown`，所以这些 key 自己写成
/// `<p align="center">…</p>` 才在 alert 里居中。
///
/// 这份清单是 `base_ui` 之外的那些 alert 决定读什么，它记在这里是因为标签对的写法只有这里能验：
/// markdown 组件认的是"行首的开标签 + 同一块里的闭标签"，写成 `<p align="center">x`（少一个闭标签）
/// 或者中间空一行，文案都会**整段消失**——不报错，只是 alert 里什么都不显示。
/// 每加一条 alert 文案就把 key 抄进来，下面那条测试保证它仍然画得出来、而且读起来没有标签。
const ALERT_MESSAGES: &[&str] = &[
    "settings_ui.save_failed.message",
    "startup.config_reset_message",
    "startup.config_reset_message_no_backup",
    "widget.tweet_monitor.add_success",
    "window.account.awaiting_authorization",
    "window.account.sign_out_confirm",
    "window.workspace.add_message",
    "window.workspace.delete_message",
    "window.workspace.name_exists",
    "window.workspace.name_required",
    "window.workspace.rename_message",
];

/// 组件正文里的占位文案：widget 没有内容时，框架的占位把 waiting 的句子交给 markdown 组件画，
/// 所以这些 key 也是自己写成居中标签对才居中。
///
/// 与 alert 那份清单同理，写法错了的代价是整段文案不显示。`common.market_not_loaded` 是这些句子里
/// 唯一带参数的：场所名与控件名由 `widget::venue` 供上，两个 `%{…}` 都落在标签对里面。
const PLACEHOLDER_MESSAGES: &[&str] = &[
    "common.market_not_loaded",
    "widget.candlestick.select_instrument_prompt",
    "widget.etf.logged_out",
    "widget.hyperliquid_wallet_monitor.empty_addresses",
    "widget.indices.logged_out",
    "widget.indices.select_at_least_one",
    "widget.market_trades.select_instrument_prompt",
    "widget.news.logged_out",
    "widget.order_book.select_instrument_prompt",
    "widget.perpetual_statistics.select_instrument_prompt",
    "widget.tweet_monitor.logged_out",
    "widget.tweet_monitor.waiting",
    "widget.wallet_monitor.empty_addresses",
    "widget.wallet_monitor.logged_out",
];

/// 安装产品自己的目录，与启动时装的是同一份。
fn install_catalogue(cx: &mut App) {
    embedded::install(cx).expect("the embedded locales should load");
}

/// 装上目录，并交出它带的语言。
fn install_and_languages(cx: &mut TestAppContext) -> Vec<LanguageCode> {
    cx.update(install_catalogue);
    let languages = cx.update(|cx| cx.global::<i18n::translator::types::I18n>().languages());
    assert_eq!(
        languages.len(),
        2,
        "the catalogue should carry both locales"
    );

    languages
}

/// 持有 `Entity<MarkdownState>` 的最小 `Render` 视图。
struct Harness {
    state: Entity<MarkdownState>,
}

impl Harness {
    fn new(cx: &mut Context<Self>, source: &str) -> Self {
        Self {
            state: cx.new(|cx| MarkdownState::new(source, cx)),
        }
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w_full().p_2().child(Markdown::new(&self.state))
    }
}

fn harness<'a>(
    cx: &'a mut TestAppContext,
    source: &str,
) -> (Entity<Harness>, &'a mut VisualTestContext) {
    cx.update(style::theme::install);
    let (view, cx) = cx.add_window_view(|_, cx| Harness::new(cx, source));
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

/// `debug_bounds` 只在真正绘制过的那一帧有值，所以读之前先强制一次完整重画。
fn settle(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

/// 当前状态里的 markdown 原文。
fn source(cx: &VisualTestContext, view: &Entity<Harness>) -> String {
    cx.read(|app| view.read(app).state.read(app).source().to_string())
}

/// 当前状态里这篇文档的纯文本投影。
fn plain_text(cx: &VisualTestContext, view: &Entity<Harness>) -> String {
    cx.read(|app| view.read(app).state.read(app).plain_text().to_string())
}

/// `new` 记住传入的原文，`set_source` 之后换成新的那一份。
#[gpui_kit::test]
fn the_source_should_round_trip_through_new_and_set_source(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, TWO_BLOCKS);

    assert_eq!(
        source(cx, &view),
        TWO_BLOCKS,
        "`new` should keep the source it was built from"
    );

    set_source(cx, &view, ONE_BLOCK);

    assert_eq!(
        source(cx, &view),
        ONE_BLOCK,
        "`set_source` should replace the source"
    );
    assert_ne!(
        source(cx, &view),
        TWO_BLOCKS,
        "the old source should not survive `set_source`"
    );
}

/// 原文没变时 `set_source` 是空操作：文档不重解析，hover 记录也不被清掉。
///
/// 这是"每帧按当前语言把消息解析成原文再交过来"的调用方（框架的占位）所依赖的那条性质：
/// 挡在这里，同一段文字才不会被每帧重解析一遍。
#[gpui_kit::test]
fn setting_the_same_source_again_should_change_nothing(cx: &mut TestAppContext) {
    let state = cx.update(|cx| {
        let state = cx.new(|cx| MarkdownState::new(ONE_BLOCK, cx));
        state.update(cx, |state, cx| state.set_hovered_link(Some(LinkId(0)), cx));
        state
    });

    cx.update(|cx| {
        state.update(cx, |state, cx| state.set_source(ONE_BLOCK, cx));

        assert_eq!(
            state.read(cx).hovered_link(),
            Some(LinkId(0)),
            "the same source leaves the document — and the state that belongs to it — as it was"
        );
    });
}

/// 换了一段真的不同的话，旧文档的一切就都作废：hover 记录被清掉。
#[gpui_kit::test]
fn setting_a_different_source_should_reset_the_document_state(cx: &mut TestAppContext) {
    let state = cx.update(|cx| {
        let state = cx.new(|cx| MarkdownState::new(ONE_BLOCK, cx));
        state.update(cx, |state, cx| state.set_hovered_link(Some(LinkId(0)), cx));
        state
    });

    cx.update(|cx| {
        state.update(cx, |state, cx| state.set_source(TWO_BLOCKS, cx));

        assert_eq!(
            state.read(cx).hovered_link(),
            None,
            "a new document has its own links, so the old hover refers to nothing"
        );
    });
}

/// 渲染结果跟着新文档走：旧文档的第二个块在新文档里不再绘制。
#[gpui_kit::test]
fn setting_a_new_source_should_change_which_blocks_are_drawn(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, TWO_BLOCKS);
    settle(cx);
    assert!(
        cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_1").is_some(),
        "`{TWO_BLOCKS:?}` should draw two blocks"
    );

    set_source(cx, &view, ONE_BLOCK);

    assert!(
        cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_0").is_some(),
        "`{ONE_BLOCK:?}` should still draw its first block"
    );
    assert!(
        cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_1").is_none(),
        "`{ONE_BLOCK:?}` has one block, so the second one should not be drawn"
    );
}

/// 被丢弃的节点不产生任何块，而标题是块。
#[gpui_kit::test]
fn dropped_constructs_should_draw_no_block(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, ONE_BLOCK);
    settle(cx);
    assert!(
        cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_0").is_some(),
        "a heading is drawn as a block"
    );

    for dropped in ["- item", "> quote", "```\ncode\n```", "<div>x</div>"] {
        set_source(cx, &view, dropped);
        assert!(
            cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_0").is_none(),
            "`{dropped:?}` is dropped whole, so it should draw no block"
        );
    }
}

/// `<p align="…">…</p>` 标签对是块：它照常画出来，而它之外的 HTML 块仍然什么都不画。
#[gpui_kit::test]
fn an_aligned_paragraph_should_draw_a_block(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "<p align=\"center\">centered</p>");
    settle(cx);
    assert!(
        cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_0").is_some(),
        "an aligned paragraph should be drawn as a block"
    );

    for dropped in ["<p align=\"justify\">x</p>", "<div>x</div>"] {
        set_source(cx, &view, dropped);
        assert!(
            cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_0").is_none(),
            "`{dropped:?}` is not the supported form, so it should draw no block"
        );
    }
}

/// `<p align="…">` 同样只是画法：读出来的是段落里的词，不是标签。
#[gpui_kit::test]
fn an_aligned_paragraph_should_read_as_the_words_inside_it(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "<p align=\"center\">centered text</p>");
    let text = plain_text(cx, &view);

    assert!(
        text.contains("centered text"),
        "`{text:?}` should read as the words of the paragraph"
    );
    for syntax in ["<p", "</p>", "align"] {
        assert!(
            !text.contains(syntax),
            "`{text:?}` should not read `{syntax}`"
        );
    }
}

/// `Markdown::new(&state)` 能嵌进一个 `Render` 视图并画出这一帧。
#[gpui_kit::test]
fn markdown_should_draw_inside_a_view(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx, TWO_BLOCKS);
    settle(cx);

    assert!(
        cx.debug_bounds("BASE_UI_MARKDOWN").is_some(),
        "the markdown column should be drawn"
    );
    assert!(
        cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_0").is_some(),
        "the heading should be drawn"
    );
    assert!(
        cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_1").is_some(),
        "the paragraph should be drawn"
    );
}

/// 强调标记只是画法：读出来的是它包着的词，不是 `*`。
#[gpui_kit::test]
fn a_bold_span_should_read_as_the_words_it_wraps(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "**bold** text");
    let text = plain_text(cx, &view);

    assert!(
        text.contains("bold text"),
        "`{text:?}` should read as the words of the paragraph"
    );
    assert!(
        !text.contains('*'),
        "`{text:?}` should not read the emphasis markers"
    );
}

/// `<kbd>` 与 `file:` 图片同样只是画法：读出来的是这一段里的词。
///
/// 这条同时钉住"可访问名称不是语法"：图片本就站在它所属的那个词旁边，读它等于读两遍。
#[gpui_kit::test]
fn a_kbd_span_should_read_as_the_words_inside_it(cx: &mut TestAppContext) {
    let (view, cx) = harness(
        cx,
        "from the <kbd>![](file://icons/phosphoricons/graph.svg) Markets</kbd> button",
    );
    let text = plain_text(cx, &view);

    assert!(
        text.contains("Markets") && text.contains("button"),
        "`{text:?}` should read the words around the image"
    );
    for syntax in ["<kbd>", "</kbd>", "file:", "graph.svg"] {
        assert!(
            !text.contains(syntax),
            "`{text:?}` should not read `{syntax}`"
        );
    }
}

/// 块与块之间读作一个换行、空行读作空行：读到的是文档自己的行结构。
#[gpui_kit::test]
fn two_blocks_should_read_as_the_lines_they_are(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, TWO_BLOCKS);
    let text = plain_text(cx, &view);

    assert_eq!(
        text, "Title\n\ntext",
        "`{TWO_BLOCKS:?}` should read as its two blocks with the blank line the source has"
    );
}

/// 块内换行读作换行：两边的词都在，而且不粘成一个词。
#[gpui_kit::test]
fn a_line_break_should_read_as_a_newline(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "a\nb");
    let text = plain_text(cx, &view);

    assert_eq!(
        text, "a\nb",
        "`{text:?}` should read the break as the newline it is"
    );
}

/// 空行在"读"的时候也是空行，而且连续几个空行就是几个：这一份投影不折叠空白。
#[gpui_kit::test]
fn blank_lines_should_read_as_blank_lines(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx, "a\n\n\n\nb");

    assert_eq!(plain_text(cx, &view), "a\n\n\n\nb");

    set_source(cx, &view, "a\nb");

    assert_eq!(plain_text(cx, &view), "a\nb");
}

/// 两条共用的断言：一条目录文案带着它写好的标签对到达组件（文案里那对是整句包住的一个元素，
/// 闭标签也必须到组件手上，否则整段文案会被当成没闭合的标签对），以及组件把它画出来之后，
/// 读到的就是那句话本身——写错的代价是整段不显示，或者把语法读出来。
///
/// 文案由目录**解析成用户看到的那一句**再检查：带参数的那几条（`common.market_not_loaded`
/// 是其中之一）交给组件的从来不是目录原文，而是 `widget::venue` 把参数填进去之后的句子，而
/// "填进去之后还成立吗"正是原文里那个 `%{…}` 看不见的东西——一个填进来的值里少了一个闭标签
/// 就会让整段消失，语法断言对它一个字都说不出。
///
/// 这份 fixture 自己填那两个参数，而不是借 `widget::venue`：`widget` 依赖 `base_ui`，反向
/// 依赖不成立。填进来的值照着真实那个值的形状写：控件名是一行，每个市场参与者一行、写成它的
/// logo（`?size=16` 的行内图片，路径就是 `brands/` 下的那一张）加上加粗的名字。
fn assert_message_draws_centered(cx: &mut TestAppContext, language: LanguageCode, key: &str) {
    let message = cx.update(|cx| {
        cx.set_global::<LanguageCode>(language);
        t!(
            cx,
            key,
            control = t!(cx, "window.market_participants.title"),
            markets = "![](file://brands/Binance/binance.png?size=16) **Binance**\n\
                       ![](file://brands/OKX/okx.png?size=16) **OKX**"
        )
    });
    assert!(
        message.starts_with("<p align=\"center\">") && message.ends_with(ALIGN_TAG_CLOSE),
        "`{key}` in {language} should reach the component as the tag pair the catalogue wrote: \
         `{message:?}`"
    );

    let (view, cx) = harness(cx, message.as_ref());
    let text = plain_text(cx, &view);
    assert!(
        !text.is_empty(),
        "`{key}` in {language} draws no paragraph at all: `{message:?}` is not read as an aligned \
         paragraph, which is what a tag pair left unclosed costs"
    );
    // 只有带列表的那几条读得出市场参与者名：标签对里的**图片与加粗**是行内语法，本来就该被读掉，
    // 剩下的词才是文案，所以断言按"这段词在这条文案里"来写，不按"每条都读得到"。
    if message.contains("brands/") {
        assert!(
            text.contains("Binance") && text.contains("OKX"),
            "`{key}` in {language} should read the market participants the list names: `{text:?}`"
        );
    }
    for syntax in ["<p", "</p>", "align", "%{", "![](", "**", "file://"] {
        assert!(
            !text.contains(syntax),
            "`{key}` in {language} should read as the sentence it is, not as `{syntax}`: `{text:?}`"
        );
    }
}

/// alert 的文案是产品自己的目录里那几句：它们在 alert 里居中，而居中的写法**只有整对标签都到齐
/// 时才成立**（标签对可以跨空行）——写错的代价是整段文案不显示，不是报错。
#[gpui_kit::test]
fn every_alert_message_should_draw_as_an_aligned_paragraph(cx: &mut TestAppContext) {
    let languages = install_and_languages(cx);

    for language in languages {
        for key in ALERT_MESSAGES {
            assert_message_draws_centered(cx, language, key);
        }
    }
}

/// widget 正文里的占位文案同样是目录里那几句，由框架的占位交给同一个组件画：写成标签对的才居中，
/// 而写坏了同样是整段不显示。
///
/// 两条断言与 alert 那条共用；只有这几句带参数，所以它们多验一件事：参数被填进去之后整段仍然成立。
#[gpui_kit::test]
fn every_placeholder_message_should_draw_as_an_aligned_paragraph(cx: &mut TestAppContext) {
    let languages = install_and_languages(cx);

    for language in languages {
        for key in PLACEHOLDER_MESSAGES {
            assert_message_draws_centered(cx, language, key);
        }
    }
}

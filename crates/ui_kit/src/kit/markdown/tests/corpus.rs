use smol_str::SmolStr;
use ui_core::{
    node::{
        id::NodeId,
        tree::{NodeKind, NodeTree},
    },
    text::{TextAlign, TextContent, TextSlant, TextStyle},
};

use super::{MarkdownStyle, TestApp, children, mount, style, tree_of};

const DESIGN: &str = include_str!("../../../../design/markdown-redesign-design.md");

const SKIPPED: &[(&str, u8, &str, &str)] = &[
    (
        "autolinks (GFM 622-635)",
        5,
        "不支持 GFM 扩展自动链接",
        "D2: extension autolinks are not implemented",
    ),
    (
        "raw inline html",
        3,
        "白名单外标签丢标签保文字",
        "D7: html mapping omits raw markup",
    ),
    (
        "raw html blocks (type 1-5)",
        3,
        "type 1–5 块整块忽略",
        "D7: type 1-5 blocks are ignored as a whole",
    ),
    ("link href", 16, "链接不保留 URL", "D9: links keep no url"),
    (
        "blank-line spacers",
        6,
        "保留源码空行占位节点",
        "D4: blank lines render as spacer nodes",
    ),
];

fn mark_html(tag: &'static str, out: &mut String) {
    match tag {
        "del" => out.push_str("<del>"),
        "strong" => out.push_str("<strong>"),
        "em" => out.push_str("<em>"),
        "kbd" => out.push_str("<kbd>"),
        "a" => out.push_str("<a>"),
        "code" => out.push_str("<code>"),
        _ => panic!("unknown tag {tag}"),
    }
}

fn close_html(tag: &str, out: &mut String) {
    match tag {
        "del" => out.push_str("</del>"),
        "strong" => out.push_str("</strong>"),
        "em" => out.push_str("</em>"),
        "kbd" => out.push_str("</kbd>"),
        "a" => out.push_str("</a>"),
        "code" => out.push_str("</code>"),
        _ => panic!("unknown tag {tag}"),
    }
}

fn escape_into(text: &str, out: &mut String) {
    for character in text.chars() {
        if character == '\u{200B}' {
            continue;
        }
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
}

fn inline_marks(
    style: &TextStyle,
    base: &TextStyle,
    markdown: &MarkdownStyle,
) -> Vec<&'static str> {
    let decorations = style.decorations();
    let background = style.background().is_some();
    let mono = style.family().map(SmolStr::as_str) == Some(super::MONO_FAMILY);
    let underline = decorations.underline;
    let mut tags = Vec::with_capacity(3);
    if decorations.strikethrough.is_some() {
        tags.push("del");
    }
    if style.weight == 700 && base.weight != 700 {
        tags.push("strong");
    }
    if style.slant == TextSlant::Italic && base.slant != TextSlant::Italic {
        tags.push("em");
    }
    let kbd = background
        && mono
        && underline.is_some_and(|value| value.color == Some(markdown.code_foreground));
    if kbd {
        tags.push("kbd");
    }
    if underline.is_some_and(|value| value.color == Some(markdown.link)) {
        tags.push("a");
    }
    if background && mono && !kbd {
        tags.push("code");
    }
    tags
}

fn emit_inline(content: &TextContent, out: &mut String) {
    let markdown = style();
    let base = &content.style;
    let mut boundaries = vec![0usize, content.text.len()];
    for run in content.runs() {
        boundaries.push(run.range.start);
        boundaries.push(run.range.end);
    }
    for inline_box in &content.inline_boxes {
        boundaries.push(inline_box.index);
    }
    boundaries.retain(|boundary| *boundary <= content.text.len());
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut stack: Vec<&'static str> = Vec::new();
    for window in boundaries.windows(2) {
        let start = window[0];
        let end = window[1];
        let tags = content
            .runs()
            .iter()
            .find(|run| run.range.start <= start && start < run.range.end)
            .map(|run| inline_marks(&run.style, base, &markdown))
            .unwrap_or_default();
        let common = stack
            .iter()
            .zip(tags.iter())
            .take_while(|(open, wanted)| open == wanted)
            .count();
        while stack.len() > common {
            let tag = stack.pop().expect("an open tag");
            close_html(tag, out);
        }
        for tag in &tags[common..] {
            mark_html(tag, out);
            stack.push(tag);
        }
        for inline_box in &content.inline_boxes {
            if inline_box.index == start {
                out.push_str("<img>");
            }
        }
        if let Some(chunk) = content.text.get(start..end) {
            escape_into(chunk, out);
        }
    }
    for inline_box in &content.inline_boxes {
        if inline_box.index == content.text.len() {
            out.push_str("<img>");
        }
    }
    while let Some(tag) = stack.pop() {
        close_html(tag, out);
    }
}

fn heading_level(style: &TextStyle) -> Option<u8> {
    let body = super::style().body_size;
    if style.weight != 700 {
        return None;
    }
    let ratio = style.size.value() / body.value();
    if ratio >= 1.7 {
        Some(1)
    } else if ratio >= 1.45 {
        Some(2)
    } else if ratio >= 1.28 {
        Some(3)
    } else if ratio >= 1.13 {
        Some(4)
    } else if ratio >= 1.03 {
        Some(5)
    } else if (ratio - 1.0).abs() < 0.01 {
        Some(6)
    } else {
        None
    }
}

fn emit_paragraph(content: &TextContent, out: &mut String) {
    match heading_level(&content.style) {
        Some(level) => {
            out.push_str(&format!("<h{level}>"));
            emit_inline(content, out);
            out.push_str(&format!("</h{level}>"));
        }
        None => {
            match content.style.alignment {
                TextAlign::Center => out.push_str("<p align=\"center\">"),
                TextAlign::Right => out.push_str("<p align=\"right\">"),
                _ => out.push_str("<p>"),
            }
            emit_inline(content, out);
            out.push_str("</p>");
        }
    }
}

fn marker_text(tree: &NodeTree, item: NodeId) -> Option<SmolStr> {
    let entry = tree.get(item).ok()?;
    for child in entry.children() {
        if tree.debug_id(*child) == Some("MARKDOWN_LIST_MARKER")
            && let NodeKind::Text(content) = tree.get(*child).ok()?.kind()
        {
            return Some(content.text.clone());
        }
    }
    None
}

fn task_state(tree: &NodeTree, item: NodeId) -> Option<bool> {
    for child in tree.get(item).ok()?.children() {
        if tree.debug_id(*child) == Some("MARKDOWN_LIST_MARKER")
            && let NodeKind::Box = tree.get(*child).ok()?.kind()
        {
            let checked = !tree.get(*child).ok()?.children().is_empty();
            return Some(checked);
        }
    }
    None
}

fn push_checkbox(checked: bool, out: &mut String) {
    if checked {
        out.push_str("<input type=\"checkbox\" disabled checked> ");
    } else {
        out.push_str("<input type=\"checkbox\" disabled> ");
    }
}

fn emit_list(app: &TestApp, node: NodeId, out: &mut String) {
    let tree = tree_of(app);
    let entry = tree.get(node).expect("the list node");
    let first_marker = entry
        .children()
        .iter()
        .find_map(|item| marker_text(tree, *item));
    let ordered = first_marker
        .as_ref()
        .is_some_and(|marker| !marker.ends_with('•'));
    let start = first_marker
        .as_deref()
        .and_then(|marker| marker.trim_end_matches('.').parse::<u32>().ok())
        .unwrap_or(1);
    if ordered {
        if start == 1 {
            out.push_str("<ol>");
        } else {
            out.push_str(&format!("<ol start=\"{start}\">"));
        }
    } else {
        out.push_str("<ul>");
    }
    for item in entry.children() {
        if tree.debug_id(*item) != Some("MARKDOWN_LIST_ITEM") {
            continue;
        }
        out.push_str("<li>");
        let task = task_state(tree, *item);
        let mut checkbox_pending = task;
        for child in tree.get(*item).expect("the item node").children() {
            match tree.debug_id(*child) {
                Some("MARKDOWN_LIST_MARKER") => {}
                Some("MARKDOWN_LIST_CONTENT" | "MARKDOWN_LIST_CONTENT_TIGHT") => {
                    let tight = tree.debug_id(*child) == Some("MARKDOWN_LIST_CONTENT_TIGHT");
                    for block in tree.get(*child).expect("the content column").children() {
                        if let NodeKind::Text(content) =
                            tree.get(*block).expect("the content block").kind()
                        {
                            if tight {
                                if let Some(checked) = checkbox_pending.take() {
                                    push_checkbox(checked, out);
                                }
                                emit_inline(content, out);
                                continue;
                            }
                            if let Some(checked) = checkbox_pending {
                                out.push_str("<p>");
                                push_checkbox(checked, out);
                                emit_inline(content, out);
                                out.push_str("</p>");
                                checkbox_pending = None;
                                continue;
                            }
                        }
                        emit_block(app, *block, out);
                    }
                }
                _ => match tree.get(*child).expect("the item child").kind() {
                    NodeKind::Text(content) => {
                        if let Some(checked) = checkbox_pending.take() {
                            push_checkbox(checked, out);
                        }
                        emit_inline(content, out);
                    }
                    _ => emit_block(app, *child, out),
                },
            }
        }
        if let Some(checked) = checkbox_pending {
            push_checkbox(checked, out);
        }
        out.push_str("</li>");
    }
    out.push_str(if ordered { "</ol>" } else { "</ul>" });
}

fn text_of(app: &TestApp, node: NodeId) -> Option<TextContent> {
    match tree_of(app).get(node).ok()?.kind() {
        NodeKind::Text(content) => Some(content.as_ref().clone()),
        _ => None,
    }
}

fn emit_table(app: &TestApp, node: NodeId, out: &mut String) {
    let tree = tree_of(app);
    let entry = tree.get(node).expect("the table node");
    let rows: Vec<NodeId> = entry
        .children()
        .iter()
        .copied()
        .filter(|child| tree.debug_id(*child) == Some("MARKDOWN_TABLE_ROW"))
        .collect();
    out.push_str("<table>");
    for (index, row) in rows.iter().enumerate() {
        let header = index == 0;
        if header {
            out.push_str("<thead><tr>");
        } else if index == 1 {
            out.push_str("<tbody><tr>");
        } else {
            out.push_str("<tr>");
        }
        for cell in tree.get(*row).expect("the row node").children() {
            if tree.debug_id(*cell) != Some("MARKDOWN_TABLE_CELL") {
                continue;
            }
            let content = tree
                .get(*cell)
                .expect("the cell node")
                .children()
                .iter()
                .find_map(|child| text_of(app, *child));
            let align = match content.as_ref().map(|value| value.style.alignment) {
                Some(TextAlign::Center) => " align=\"center\"",
                Some(TextAlign::Right) => " align=\"right\"",
                Some(TextAlign::Left) => " align=\"left\"",
                _ => "",
            };
            let tag = if header { "th" } else { "td" };
            out.push_str(&format!("<{tag}{align}>"));
            if let Some(content) = content {
                emit_inline(&content, out);
            }
            out.push_str(&format!("</{tag}>"));
        }
        out.push_str("</tr>");
        if header {
            out.push_str("</thead>");
        }
    }
    if rows.len() > 1 {
        out.push_str("</tbody>");
    }
    out.push_str("</table>");
}

fn emit_block(app: &TestApp, node: NodeId, out: &mut String) {
    let tree = tree_of(app);
    let entry = tree.get(node).expect("the block node");
    let id = tree.debug_id(node);
    if id == Some("MARKDOWN_CODE")
        && let NodeKind::Text(content) = entry.kind()
    {
        out.push_str("<pre><code>");
        escape_into(&content.text, out);
        out.push_str("</code></pre>");
        return;
    }
    match entry.kind() {
        NodeKind::Text(content) => emit_paragraph(content, out),
        NodeKind::Box => match id {
            Some("MARKDOWN_SPACER" | "MARKDOWN_TABLE_RULE" | "MARKDOWN_QUOTE_BAR") => {}
            Some("MARKDOWN_QUOTE") => {
                out.push_str("<blockquote>");
                for child in entry.children() {
                    if tree.debug_id(*child) == Some("MARKDOWN_QUOTE_BODY") {
                        for block in tree.get(*child).expect("the quote body").children() {
                            emit_block(app, *block, out);
                        }
                    }
                }
                out.push_str("</blockquote>");
            }
            Some("MARKDOWN_LIST") => emit_list(app, node, out),
            Some("MARKDOWN_CODE") => {
                out.push_str("<pre><code>");
                for child in entry.children() {
                    if let NodeKind::Text(content) = tree.get(*child).expect("the code text").kind()
                    {
                        escape_into(&content.text, out);
                    }
                }
                out.push_str("</code></pre>");
            }
            Some("MARKDOWN_TABLE") => emit_table(app, node, out),
            Some("MARKDOWN_RULE") => out.push_str("<hr>"),
            _ => {
                for child in entry.children() {
                    emit_block(app, *child, out);
                }
            }
        },
        _ => {}
    }
}

fn html_of(app: &TestApp, root: NodeId) -> String {
    let mut out = String::new();
    for child in children(app, root) {
        emit_block(app, child, &mut out);
    }
    out
}

fn render(source: &str) -> String {
    let (app, node) = mount(source);
    html_of(&app, node.root())
}

fn assert_corpus(corpus: &[(&str, &str, String)], expected_count: usize) {
    assert_eq!(corpus.len(), expected_count);
    for (case, source, expected) in corpus {
        assert_eq!(render(source), *expected, "{case}");
    }
}

fn deletion_corpus() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "del:basic",
            "~~strike~~",
            "<p><del>strike</del></p>".to_owned(),
        ),
        ("del:middle", "a~~b~~c", "<p>a<del>b</del>c</p>".to_owned()),
        (
            "del:em-inside",
            "~~*em*~~",
            "<p><del><em>em</em></del></p>".to_owned(),
        ),
        (
            "del:strong-inside",
            "~~**strong**~~",
            "<p><del><strong>strong</strong></del></p>".to_owned(),
        ),
        (
            "del:unbalanced-tail",
            "~~a~~b~~",
            "<p><del>a</del>b~~</p>".to_owned(),
        ),
        (
            "del:across-a-softbreak",
            "~~a\nb~~",
            "<p><del>a\nb</del></p>".to_owned(),
        ),
    ]
}

fn block_corpus() -> Vec<(&'static str, &'static str, String)> {
    vec![
        ("block:atx", "# ATX", "<h1>ATX</h1>".to_owned()),
        ("block:atx-closing", "## ATX ##", "<h2>ATX</h2>".to_owned()),
        (
            "block:setext-one",
            "Setext\n====",
            "<h1>Setext</h1>".to_owned(),
        ),
        (
            "block:setext-two",
            "Setext\n----",
            "<h2>Setext</h2>".to_owned(),
        ),
        (
            "block:quote",
            "> a\n> b",
            "<blockquote><p>a\nb</p></blockquote>".to_owned(),
        ),
        (
            "block:nested-quote",
            "> a\n>\n> > b",
            "<blockquote><p>a</p><blockquote><p>b</p></blockquote></blockquote>".to_owned(),
        ),
        (
            "block:tight-bullet-list",
            "- a\n- b",
            "<ul><li>a</li><li>b</li></ul>".to_owned(),
        ),
        (
            "block:loose-bullet-list",
            "- a\n\n- b",
            "<ul><li><p>a</p></li><li><p>b</p></li></ul>".to_owned(),
        ),
        (
            "block:ordered-list-start",
            "3. a\n4. b",
            "<ol start=\"3\"><li>a</li><li>b</li></ol>".to_owned(),
        ),
        (
            "block:loose-nested-bullet-list",
            "- a\n\n  - b",
            "<ul><li><p>a</p><ul><li>b</li></ul></li></ul>".to_owned(),
        ),
        (
            "block:fenced-code",
            "```js\nx\n```",
            "<pre><code>x</code></pre>".to_owned(),
        ),
        (
            "block:indented-code",
            "    x\n",
            "<pre><code>x</code></pre>".to_owned(),
        ),
        ("block:rule", "---", "<hr>".to_owned()),
        (
            "block:link-reference-definition",
            "[a]: /u\n\nb",
            "<p>b</p>".to_owned(),
        ),
        (
            "block:blank-lines-at-the-edges",
            "\n\n# a\n\n",
            "<h1>a</h1>".to_owned(),
        ),
    ]
}

fn inline_corpus() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "inline:backslash-escape",
            "\\*not em\\*",
            "<p>*not em*</p>".to_owned(),
        ),
        ("inline:entity", "&amp; &#35;", "<p>&amp; #</p>".to_owned()),
        (
            "inline:kbd",
            "<kbd>K</kbd>",
            "<p><kbd>K</kbd></p>".to_owned(),
        ),
        (
            "inline:code-span-backticks",
            "`` a`b ``",
            "<p><code>a`b</code></p>".to_owned(),
        ),
        (
            "inline:code-span-spaces",
            "` a `",
            "<p><code>a</code></p>".to_owned(),
        ),
        (
            "inline:file-image",
            "![](file://icons/x.png)",
            "<p><img></p>".to_owned(),
        ),
        (
            "inline:image-alt-falls-back",
            "![alt](https://example.com/a.png)",
            "<p>alt</p>".to_owned(),
        ),
        (
            "inline:strong-with-em",
            "**strong *both***",
            "<p><strong>strong <em>both</em></strong></p>".to_owned(),
        ),
        ("inline:em", "*em*", "<p><em>em</em></p>".to_owned()),
        ("inline:link", "[a](u)", "<p><a>a</a></p>".to_owned()),
        (
            "inline:link-inside-strong",
            "**[a](u)**",
            "<p><strong><a>a</a></strong></p>".to_owned(),
        ),
        (
            "inline:code-inside-link",
            "[`x`](u)",
            "<p><a><code>x</code></a></p>".to_owned(),
        ),
    ]
}

fn alignment_corpus() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "align:center",
            "<p align=\"center\">a</p>",
            "<p align=\"center\">a</p>".to_owned(),
        ),
        (
            "align:right",
            "<p align=\"right\">a</p>",
            "<p align=\"right\">a</p>".to_owned(),
        ),
        (
            "align:left",
            "<p align=\"left\">a</p>",
            "<p>a</p>".to_owned(),
        ),
    ]
}

fn degenerate_corpus() -> Vec<(&'static str, String, String)> {
    let deep_quote = format!("{}x", "> ".repeat(200));
    let deep_quote_html = format!(
        "{}<p>x</p>{}",
        "<blockquote>".repeat(64),
        "</blockquote>".repeat(64)
    );
    let deep_emphasis = format!("{}x{}", "*".repeat(160), "*".repeat(160));
    let deep_list = format!("{}x", "- ".repeat(300));
    let deep_list_html = format!("{}x{}", "<ul><li>".repeat(32), "</li></ul>".repeat(32));
    let long_paragraph = "a".repeat(10_000);
    let long_paragraph_html = format!("<p>{}</p>", "a".repeat(10_000));
    vec![
        ("degenerate:deep-quote", deep_quote, deep_quote_html),
        (
            "degenerate:deep-emphasis",
            deep_emphasis,
            "<p><strong>x</strong></p>".to_owned(),
        ),
        ("degenerate:deep-list", deep_list, deep_list_html),
        (
            "degenerate:long-paragraph",
            long_paragraph,
            long_paragraph_html,
        ),
        ("degenerate:empty-document", String::new(), String::new()),
        (
            "degenerate:whitespace-only",
            "   \n\n \n".to_owned(),
            String::new(),
        ),
    ]
}

#[test]
fn the_gfm_strikethrough_corpus_emits_canonical_html() {
    assert_corpus(&deletion_corpus(), 6);
}

#[test]
fn the_gfm_block_corpus_emits_canonical_html() {
    assert_corpus(&block_corpus(), 15);
}

#[test]
fn the_gfm_inline_corpus_emits_canonical_html() {
    assert_corpus(&inline_corpus(), 12);
}

#[test]
fn the_gfm_alignment_corpus_emits_canonical_html() {
    assert_corpus(&alignment_corpus(), 3);
    let (left_app, left_node) = mount("<p>x</p>");
    let left_x = super::glyph_run(&left_app, super::children(&left_app, left_node.root())[0]).0;
    let (center_app, center_node) = mount("<p align=\"center\">x</p>");
    let center_x = super::glyph_run(
        &center_app,
        super::children(&center_app, center_node.root())[0],
    )
    .0;
    let (right_app, right_node) = mount("<p align=\"right\">x</p>");
    let right_x = super::glyph_run(
        &right_app,
        super::children(&right_app, right_node.root())[0],
    )
    .0;
    assert_eq!(left_x, ui_core::math::px(0.0));
    assert!(center_x > left_x);
    assert!(right_x > center_x);
}

#[test]
fn the_degenerate_corpus_keeps_the_model_normalized_output() {
    let corpus = degenerate_corpus();
    assert_eq!(corpus.len(), 6);
    for (case, source, expected) in corpus {
        assert_eq!(render(&source), expected, "{case}");
    }
}

#[test]
fn the_conformance_corpus_skips_are_registered_deviations() {
    let start = DESIGN.find("### 10.1").expect("the deviation chapter");
    let end = DESIGN.find("### 10.2").expect("the risk chapter");
    let deviations = &DESIGN[start..end];
    let row = |number: u8| {
        let marker = format!("| {number} |");
        deviations
            .lines()
            .find(|line| line.starts_with(&marker))
            .unwrap_or_else(|| panic!("the deviation {number} is registered"))
    };
    assert_eq!(SKIPPED.len(), 5);
    let mut numbers = Vec::new();
    for (case, number, keyword, reason) in SKIPPED {
        assert!(!reason.trim().is_empty(), "{case}");
        let line = row(*number);
        assert!(
            line.contains(keyword),
            "{case} cites §10.1-{number}, whose row lacks {keyword:?}: {line}"
        );
        numbers.push(*number);
    }
    numbers.sort_unstable();
    assert_eq!(numbers, vec![3, 3, 5, 6, 16]);
}

fn table_corpus() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "table:two-columns",
            "| foo | bar |\n|:--|:--|",
            r#"<table><thead><tr><th align="left">foo</th><th align="left">bar</th></tr></thead></table>"#.to_owned(),
        ),
        (
            "table:center-and-right",
            "| abc | defghi |\n|:-:|--:|",
            r#"<table><thead><tr><th align="center">abc</th><th align="right">defghi</th></tr></thead></table>"#.to_owned(),
        ),
        (
            "table:header-and-body",
            "| h1 | h2 |\n|:--|--:|\n| 1 | 2 |",
            r#"<table><thead><tr><th align="left">h1</th><th align="right">h2</th></tr></thead><tbody><tr><td align="left">1</td><td align="right">2</td></tr></tbody></table>"#.to_owned(),
        ),
        (
            "table:empty-cell",
            "| a |  | c |\n|:--|:--|:--|",
            r#"<table><thead><tr><th align="left">a</th><th></th><th align="left">c</th></tr></thead></table>"#.to_owned(),
        ),
        (
            "table:escaped-pipe",
            "| f\\|oo |\n|:--|",
            r#"<table><thead><tr><th align="left">f|oo</th></tr></thead></table>"#.to_owned(),
        ),
        (
            "table:code-span-pipe",
            "| `b\\|az` |\n|:--|",
            r#"<table><thead><tr><th align="left"><code>b|az</code></th></tr></thead></table>"#.to_owned(),
        ),
        (
            "table:inline-link",
            "| [x](u) |\n|:--|",
            r#"<table><thead><tr><th align="left"><a>x</a></th></tr></thead></table>"#.to_owned(),
        ),
        (
            "table:inline-image",
            "| ![](file://icons/x.png) |\n|:--|",
            r#"<table><thead><tr><th align="left"><img></th></tr></thead></table>"#.to_owned(),
        ),
        (
            "table:short-row-is-padded",
            "| a | b |\n|:--|:--|\n| 1 |",
            r#"<table><thead><tr><th align="left">a</th><th align="left">b</th></tr></thead><tbody><tr><td align="left">1</td><td></td></tr></tbody></table>"#.to_owned(),
        ),
        (
            "table:only-a-header",
            "| only |\n|:--|",
            r#"<table><thead><tr><th align="left">only</th></tr></thead></table>"#.to_owned(),
        ),
        (
            "table:body-marks",
            "| h |\n|:--|\n| **b** |",
            r#"<table><thead><tr><th align="left">h</th></tr></thead><tbody><tr><td align="left"><strong>b</strong></td></tr></tbody></table>"#.to_owned(),
        ),
        (
            "table:inside-a-list-item",
            "- | a | b |\n  |:--|:--|\n  | 1 | 2 |",
            r#"<ul><li><table><thead><tr><th align="left">a</th><th align="left">b</th></tr></thead><tbody><tr><td align="left">1</td><td align="left">2</td></tr></tbody></table></li></ul>"#.to_owned(),
        ),
    ]
}

fn task_corpus() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "task:tight-unchecked-and-checked",
            "- [ ] a\n- [x] b",
            r#"<ul><li><input type="checkbox" disabled> a</li><li><input type="checkbox" disabled checked> b</li></ul>"#.to_owned(),
        ),
        (
            "task:uppercase-x",
            "- [X] a",
            r#"<ul><li><input type="checkbox" disabled checked> a</li></ul>"#.to_owned(),
        ),
        (
            "task:loose-items",
            "- [ ] a\n\n- [x] b",
            r#"<ul><li><p><input type="checkbox" disabled> a</p></li><li><p><input type="checkbox" disabled checked> b</p></li></ul>"#.to_owned(),
        ),
        (
            "task:nested-tight",
            "- [ ] a\n  - [x] b",
            r#"<ul><li><input type="checkbox" disabled> a<ul><li><input type="checkbox" disabled checked> b</li></ul></li></ul>"#.to_owned(),
        ),
        (
            "task:empty-item-stays-text",
            "- [ ]",
            "<ul><li>[ ]</li></ul>".to_owned(),
        ),
        (
            "task:mixed-with-a-plain-item",
            "- [ ] a\n- b",
            r#"<ul><li><input type="checkbox" disabled> a</li><li>b</li></ul>"#.to_owned(),
        ),
        (
            "task:three-items",
            "- [ ] a\n- [x] b\n- [ ] c",
            r#"<ul><li><input type="checkbox" disabled> a</li><li><input type="checkbox" disabled checked> b</li><li><input type="checkbox" disabled> c</li></ul>"#.to_owned(),
        ),
        (
            "task:beside-an-ordered-list",
            "- [ ] a\n\n1. b",
            r#"<ul><li><input type="checkbox" disabled> a</li></ul><ol><li>b</li></ol>"#.to_owned(),
        ),
        (
            "task:marker-is-not-text",
            "- [x] done",
            r#"<ul><li><input type="checkbox" disabled checked> done</li></ul>"#.to_owned(),
        ),
    ]
}

fn html_corpus() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "html:self-contained-pair",
            "<p align=\"center\">x</p>",
            r#"<p align="center">x</p>"#.to_owned(),
        ),
        (
            "html:production-multiline-pair",
            "<p align=\"center\">This widget needs the metadata of a supported market participant to be loaded first.\nLoad the metadata of at least one supported market participant from the <kbd>![](file://icons/phosphoricons/bank.svg)</kbd> button in the top left corner.\nThe market participants this widget supports are:\n%{markets}</p>",
            "<p align=\"center\">This widget needs the metadata of a supported market participant to be loaded first.\nLoad the metadata of at least one supported market participant from the <img> button in the top left corner.\nThe market participants this widget supports are:\n%{markets}</p>".to_owned(),
        ),
        (
            "html:paragraph-without-attribute",
            "<p>x</p>",
            "<p>x</p>".to_owned(),
        ),
        (
            "html:left-pair",
            "<p align=\"left\">x</p>",
            "<p>x</p>".to_owned(),
        ),
        (
            "html:right-pair",
            "<p align=\"right\">x</p>",
            r#"<p align="right">x</p>"#.to_owned(),
        ),
        (
            "html:two-pairs-in-one-block",
            "<p>a</p><p>b",
            "<p>a</p><p>b</p>".to_owned(),
        ),
        (
            "html:pair-across-blank-lines",
            "<p align=\"center\">a\n\nb</p>",
            r#"<p align="center">a</p><p align="center">b</p>"#.to_owned(),
        ),
        (
            "html:pair-across-blocks",
            "<p align=\"center\">\n# H\n\ntext\n</p>",
            r#"<h1>H</h1><p align="center">text</p>"#.to_owned(),
        ),
        (
            "html:multiple-blank-lines-in-a-pair",
            "<p align=\"center\">a\n\n\n\nb</p>",
            r#"<p align="center">a</p><p align="center">b</p>"#.to_owned(),
        ),
        (
            "html:nested-pair-windows",
            "<p align=\"center\"><p align=\"right\">x\n\ny</p></p>\n\nz",
            r#"<p align="right">x</p><p align="right">y</p><p>z</p>"#.to_owned(),
        ),
        (
            "html:unclosed-pair-keeps-its-alignment",
            "<p align=\"center\">a",
            r#"<p align="center">a</p>"#.to_owned(),
        ),
        (
            "html:stray-closing-tag-is-dropped",
            "<p>a</p></p>",
            "<p>a</p>".to_owned(),
        ),
        (
            "html:literal-closing-tag-ends-the-window",
            "<p align=\"center\">a</p>\n\nb",
            "<p align=\"center\">a</p><p>b</p>".to_owned(),
        ),
        (
            "html:division-container-parses-markdown",
            "<div>\n**bold**\n</div>",
            "<p><strong>bold</strong></p>".to_owned(),
        ),
        (
            "html:center-container-centers-its-content",
            "<center>c</center>",
            r#"<p align="center">c</p>"#.to_owned(),
        ),
        (
            "html:section-container-scopes-its-content",
            "<section>s</section>",
            "<p>s</p>".to_owned(),
        ),
        (
            "html:crossed-containers-keep-their-text",
            "<div><p>a</div>b",
            "<p>a</p><p>b</p>".to_owned(),
        ),
        (
            "html:heading-and-rule-blocks",
            "<h1>H</h1>\n\n<hr>",
            "<h1>H</h1><hr>".to_owned(),
        ),
        (
            "html:non-exact-attributes-keep-only-text",
            "<p align='center'>x</p>",
            "<p>x</p>".to_owned(),
        ),
        (
            "html:comment-is-not-a-pair",
            "<!-- <p align=\"center\">x</p> -->\ny",
            "<p>y</p>".to_owned(),
        ),
        (
            "html:crlf-line-endings",
            "a\r\n\r\nb",
            "<p>a</p><p>b</p>".to_owned(),
        ),
        (
            "html:type-one-to-five-blocks-are-ignored",
            "<pre><p align=\"center\">x</p></pre>",
            String::new(),
        ),
        (
            "html:inline-bold-and-italic",
            "<b>**b**</b> _i_",
            "<p><strong>b</strong> <em>i</em></p>".to_owned(),
        ),
        (
            "html:inline-del-code-and-break",
            "<del>d</del> <code>c</code><br>e",
            "<p><del>d</del> <code>c</code>\ne</p>".to_owned(),
        ),
        (
            "html:inline-anchor-and-image",
            "<a href=\"u\">a</a> ![](file://icons/x.png)",
            "<p><a>a</a> <img></p>".to_owned(),
        ),
        (
            "html:kbd-is-exact-only",
            "<kbd>K</kbd> <KBD>X</KBD>",
            "<p><kbd>K</kbd> X</p>".to_owned(),
        ),
        (
            "html:deep-block-loses-its-tags",
            "> <p align=\"center\">x</p>",
            "<blockquote><p>x</p></blockquote>".to_owned(),
        ),
    ]
}

#[test]
fn the_gfm_table_corpus_emits_canonical_html() {
    assert_corpus(&table_corpus(), 12);
}

#[test]
fn the_gfm_task_list_corpus_emits_canonical_html() {
    assert_corpus(&task_corpus(), 9);
}

#[test]
fn the_html_subset_corpus_emits_canonical_html() {
    assert_corpus(&html_corpus(), 27);
}

#[test]
fn the_gfm_conformance_corpus_covers_all_ninety_cases() {
    let total = deletion_corpus().len()
        + block_corpus().len()
        + inline_corpus().len()
        + alignment_corpus().len()
        + table_corpus().len()
        + task_corpus().len()
        + html_corpus().len()
        + degenerate_corpus().len();
    assert_eq!(total, 90);
}

#[test]
fn the_emitter_flattens_mark_nesting_the_model_cannot_distinguish() {
    let double = render("****x****");
    let single = render("**x**");
    assert_eq!(double, single);
    assert_eq!(double, "<p><strong>x</strong></p>");

    let del_strong = render("~~**x**~~");
    let strong_del = render("**~~x~~**");
    assert_eq!(del_strong, "<p><del><strong>x</strong></del></p>");
    assert_eq!(strong_del, del_strong);
}

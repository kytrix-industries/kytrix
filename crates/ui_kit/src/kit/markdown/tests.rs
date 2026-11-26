use std::borrow::Cow;

use smol_str::SmolStr;
use ui_core::{
    math::{LogicalPx, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        tree::{NodeKind, NodeTree},
    },
    paint::{color::ColorResolver, style_builder::StyleBuilder},
    test_support::{app::TestApp, probe::Selector, record::PrimitiveKind},
    text::{FontStack, InlineBoxPaint, TextAlign, TextContent, TextStyle, TextWrap},
};

use super::{MarkdownAssets, MarkdownNode, MarkdownStyle, inline::MONO_FAMILY};
use crate::theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role};

mod corpus;
mod fixtures;
mod gates;
mod ledger;
mod outline;
mod plain;

const EN_RELEASE_NOTES: &str = include_str!("../../../../../release_notes/v0.0.3/en.md");
const ZH_RELEASE_NOTES: &str = include_str!("../../../../../release_notes/v0.0.3/zh-CN.md");

struct EmbeddedAssets;

impl MarkdownAssets for EmbeddedAssets {
    fn load(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        crate::kit::assets::bytes(path).map(Cow::Borrowed)
    }
}

fn style() -> MarkdownStyle {
    MarkdownStyle::from_theme(&Theme::of(Appearance::Light))
}

fn mount(source: &str) -> (TestApp, MarkdownNode) {
    let mut app = TestApp::new();
    let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    let node = MarkdownNode::mount(app.app_mut(), window, root, theme, source, EmbeddedAssets)
        .expect("the markdown mounts");
    let _ = app.drain().expect("first frame");
    (app, node)
}

fn tree_of(app: &TestApp) -> &NodeTree {
    app.app().window(app.window_id()).expect("window").tree()
}

fn children(app: &TestApp, node: NodeId) -> Vec<NodeId> {
    tree_of(app)
        .get(node)
        .expect("node alive")
        .children()
        .to_vec()
}

fn texts(app: &TestApp, root: NodeId) -> Vec<TextContent> {
    let tree = tree_of(app);
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let entry = tree.get(node).expect("node alive");
        if let NodeKind::Text(content) = entry.kind() {
            found.push(content.as_ref().clone());
        }
        stack.extend(entry.children().iter().rev().copied());
    }
    found
}

fn node_count(app: &TestApp, root: NodeId) -> usize {
    let tree = tree_of(app);
    let mut count = 0;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        count += 1;
        stack.extend(tree.get(node).expect("node alive").children());
    }
    count
}

fn bounds(app: &TestApp, node: NodeId) -> ui_core::math::LogicalRect {
    app.probe().bounds(node).expect("laid out node")
}

fn gap_after(app: &TestApp, upper: NodeId, lower: NodeId) -> LogicalPx {
    let upper = bounds(app, upper);
    let lower = bounds(app, lower);
    lower.origin.y - (upper.origin.y + upper.size.height)
}

fn count(app: &TestApp, id: &'static str) -> usize {
    app.probe().count(Selector::Id(id))
}

fn node_of(app: &TestApp, id: &'static str) -> NodeId {
    app.probe().by_selector(Selector::Id(id)).expect(id)
}

#[test]
fn a_paragraph_mounts_one_text_node_with_layout_and_glyphs() {
    let (app, node) = mount("hello markdown");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "hello markdown");
    assert_eq!(node_count(&app, node.root()), 2);

    let text_node = children(&app, node.root())[0];
    assert!(bounds(&app, text_node).size.width > px(0.0));
    let record = app.frame_record();
    let frame = record.frame.as_ref().expect("the first frame renders");
    assert!(
        frame.primitives.iter().any(|primitive| {
            primitive.node == text_node
                && matches!(primitive.kind, PrimitiveKind::GlyphRun)
                && primitive.bounds.size.width > px(0.0)
        }),
        "the paragraph must produce glyphs"
    );
}

#[test]
fn an_empty_source_mounts_only_the_root() {
    let (app, node) = mount("");
    assert_eq!(node_count(&app, node.root()), 1);
    assert!(texts(&app, node.root()).is_empty());
    assert_eq!(node_of(&app, "MARKDOWN"), node.root());
}

#[test]
fn headings_scale_with_their_level_and_stay_bold_and_left() {
    let (app, node) = mount("# One\n\n###### Six");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    let body = style().body_size;
    assert_eq!(content[0].style.size, body * 1.75);
    assert_eq!(content[1].style.size, body);
    assert_eq!(content[0].style.weight, 700);
    assert_eq!(content[1].style.weight, 700);
    assert_eq!(content[0].style.alignment, TextAlign::Left);
}

#[test]
fn blank_lines_become_one_spacer_each() {
    for (source, expected) in [
        ("a\n\nb", 1),
        ("a\n\n\n\nb", 3),
        ("a\r\n\r\nb", 1),
        ("a\nb", 0),
        ("a\n", 0),
    ] {
        let (app, _node) = mount(source);
        assert_eq!(count(&app, "MARKDOWN_SPACER"), expected, "{source:?}");
    }

    let (app, node) = mount("a\n\nb");
    let spacer = node_of(&app, "MARKDOWN_SPACER");
    assert!((bounds(&app, spacer).size.height - style().line_height()).abs() < px(0.01));
    let blocks = children(&app, node.root());
    assert_eq!(blocks.len(), 3);
    assert!(gap_after(&app, blocks[1], blocks[2]) < px(0.01));
}

#[test]
fn blocks_take_the_block_gap_and_headings_take_the_extra_space() {
    let (app, node) = mount("# A\ntext");
    let blocks = children(&app, node.root());
    assert_eq!(blocks.len(), 2);
    assert!((gap_after(&app, blocks[0], blocks[1]) - px(8.0)).abs() < px(0.01));

    let (app, node) = mount("# A\n\n# B");
    let blocks = children(&app, node.root());
    assert_eq!(blocks.len(), 3);
    assert!((gap_after(&app, blocks[1], blocks[2]) - px(8.0)).abs() < px(0.01));

    let (app, node) = mount("a\n\nb");
    let blocks = children(&app, node.root());
    assert!(gap_after(&app, blocks[1], blocks[2]) < px(0.01));

    let (app, node) = mount("a\n# B");
    let blocks = children(&app, node.root());
    assert_eq!(blocks.len(), 2);
    assert!((gap_after(&app, blocks[0], blocks[1]) - px(16.0)).abs() < px(0.01));
}

#[test]
fn tight_lists_mount_markers_without_a_content_column() {
    let (app, node) = mount("- a\n- b");
    assert_eq!(count(&app, "MARKDOWN_LIST"), 1);
    assert_eq!(count(&app, "MARKDOWN_LIST_ITEM"), 2);
    assert_eq!(count(&app, "MARKDOWN_LIST_MARKER"), 2);
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 0);
    let content = texts(&app, node.root());
    let markers = content.iter().filter(|text| text.text == "•").count();
    assert_eq!(markers, 2);
    assert_eq!(node_count(&app, node.root()), 8);
}

#[test]
fn loose_lists_wrap_each_item_in_a_content_column() {
    let (app, _node) = mount("- a\n\n- b");
    assert_eq!(count(&app, "MARKDOWN_LIST_ITEM"), 2);
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 2);
}

#[test]
fn ordered_markers_start_at_the_parsed_number() {
    let (app, node) = mount("3. x\n4. y");
    let markers: Vec<SmolStr> = texts(&app, node.root())
        .into_iter()
        .map(|content| content.text)
        .filter(|text| text.ends_with('.'))
        .collect();
    assert_eq!(
        markers,
        vec![SmolStr::new_static("3."), SmolStr::new_static("4.")]
    );
}

#[test]
fn quotes_mount_a_bar_a_content_column_and_nest() {
    let (app, node) = mount("> quoted");
    assert_eq!(count(&app, "MARKDOWN_QUOTE"), 1);
    assert_eq!(count(&app, "MARKDOWN_QUOTE_BAR"), 1);
    assert_eq!(count(&app, "MARKDOWN_QUOTE_BODY"), 1);
    assert_eq!(texts(&app, node.root())[0].text, "quoted");

    let (app, _node) = mount("> outer\n>\n> > inner");
    assert_eq!(count(&app, "MARKDOWN_QUOTE"), 2);
    assert_eq!(count(&app, "MARKDOWN_QUOTE_BODY"), 2);
}

#[test]
fn code_blocks_mount_one_text_node_with_background_padding_and_mono() {
    let (app, node) = mount("```rust\nlet x = 1;\n```");
    assert_eq!(count(&app, "MARKDOWN_CODE"), 1);
    assert_eq!(count(&app, "MARKDOWN_CODE_TEXT"), 0);
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "let x = 1;");
    assert_eq!(
        content[0].style.family().map(SmolStr::as_str),
        Some(MONO_FAMILY)
    );
    assert_eq!(content[0].style.color, style().code_foreground);

    let code = node_of(&app, "MARKDOWN_CODE");
    let code_bounds = bounds(&app, code);
    assert!((code_bounds.size.width - px(800.0)).abs() < px(0.01));
    assert!(
        (code_bounds.size.height - style().line_height() - px(16.0)).abs() < px(0.01),
        "the text box is the line box plus two 8px paddings"
    );

    let quad = app
        .rendered()
        .expect("the first frame renders")
        .quads
        .iter()
        .find(|quad| quad.node == code)
        .expect("the code background is painted");
    assert_eq!(
        quad.bounds, code_bounds,
        "the background covers the border box"
    );
    assert_eq!(quad.corner_radius, [px(4.0); 4]);
    assert_eq!(quad.fill.components, resolved(style().code_background));

    let (app, node) = mount("    indented\n");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "indented");
}

#[test]
fn a_code_block_wraps_at_its_padded_content_width() {
    let long = "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll mmmm nnnn oooo pppp qqqq rrrr ssss tttt";
    let source = format!("```\n{long}\n```");
    let (mut app, node) = mount(&source);
    let code = node_of(&app, "MARKDOWN_CODE");
    let code_bounds = bounds(&app, code);

    let window = app.window_id();
    let mono = style();
    let comparison = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_col()
                .width(px(800.0))
                .padding(px(8.0))
                .build(),
        )
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            node.root(),
        )
        .expect("comparison container");
    let text_style = TextStyle::new(mono.body_size)
        .with_family(SmolStr::new_static(MONO_FAMILY))
        .with_color(mono.code_foreground)
        .with_line_height(mono.line_height)
        .with_alignment(TextAlign::Left)
        .with_wrap(TextWrap::Wrap);
    let inner = NodeBuilder::text(SmolStr::new(long), text_style)
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            comparison,
        )
        .expect("comparison text");
    let _ = app.drain().expect("comparison frame");

    let inner_bounds = bounds(&app, inner);
    assert!(
        (code_bounds.size.width - px(16.0) - inner_bounds.size.width).abs() < px(0.01),
        "the padded text node wraps to the same content width as the container"
    );
    assert!(
        (bounds(&app, code).size.height - px(16.0) - inner_bounds.size.height).abs() < px(0.01),
        "the padded text node takes the same line count as the container"
    );
}

#[test]
fn a_code_block_settles_into_an_idle_frame() {
    let (mut app, _node) = mount("```rust\nlet x = 1;\n```");
    let _ = app.drain().expect("second frame");
    assert!(
        !app.frame_record().rendered,
        "a code block must not keep re-laying out after the first frame"
    );
}

#[test]
fn tables_mount_rows_cells_and_one_rule_per_row() {
    let (app, node) = mount("| a | b |\n|---|---|\n| 1 | 2 |");
    assert_eq!(count(&app, "MARKDOWN_TABLE"), 1);
    assert_eq!(count(&app, "MARKDOWN_TABLE_ROW"), 2);
    assert_eq!(count(&app, "MARKDOWN_TABLE_CELL"), 4);
    assert_eq!(count(&app, "MARKDOWN_TABLE_RULE"), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 4);
    assert_eq!(content[0].text, "a");
    assert_eq!(content[0].style.weight, 700);
    assert_eq!(content[1].style.weight, 700);
    assert_eq!(content[2].text, "1");
    assert_eq!(content[2].style.weight, 400);
}

#[test]
fn table_cells_take_the_column_alignment_and_pad_missing_cells() {
    let (app, node) = mount("| a | b |\n|:--|--:|\n| 1 | 2 |");
    let content = texts(&app, node.root());
    assert_eq!(content[2].style.alignment, TextAlign::Left);
    assert_eq!(content[3].style.alignment, TextAlign::Right);

    let (app, node) = mount("| a | b |\n|---|---|\n| 1 |");
    assert_eq!(count(&app, "MARKDOWN_TABLE_CELL"), 4);
    assert_eq!(texts(&app, node.root()).len(), 3);
}

#[test]
fn thematic_breaks_mount_a_one_pixel_rule() {
    let (app, _node) = mount("---");
    assert_eq!(count(&app, "MARKDOWN_RULE"), 1);
    assert!((bounds(&app, node_of(&app, "MARKDOWN_RULE")).size.height - px(1.0)).abs() < px(0.01));
}

#[test]
fn inline_marks_cut_runs_and_leave_plain_text_runless() {
    let (app, node) = mount("**bold** and `code` and ~~gone~~ and [link](https://example.com)");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "bold and code and gone and link");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 4);
    assert_eq!(runs[0].range, 0..4);
    assert_eq!(runs[0].style.weight, 700);
    assert_eq!(runs[1].range, 9..13);
    assert_eq!(
        runs[1].style.family().map(SmolStr::as_str),
        Some(MONO_FAMILY)
    );
    assert!(runs[1].style.background().is_some());
    assert_eq!(runs[2].range, 18..22);
    assert!(runs[2].style.decorations().strikethrough.is_some());
    assert_eq!(runs[3].range, 27..31);
    assert!(runs[3].style.decorations().underline.is_some());
    assert_eq!(runs[3].style.color, style().link);
}

#[test]
fn inline_kbd_and_br_cut_a_kbd_run_and_a_break() {
    let (app, node) = mount("<kbd>Ctrl</kbd><br>K");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "Ctrl\nK");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].range, 0..4);
    assert_eq!(
        runs[0].style.family().map(SmolStr::as_str),
        Some(MONO_FAMILY)
    );
    assert!(runs[0].style.decorations().underline.is_some());
    assert!(runs[0].style.background().is_some());
}

#[test]
fn file_images_become_painted_inline_boxes() {
    let (app, node) = mount("![x](file://brands/Binance/binance.png?size=24)");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "\u{200B}");
    assert_eq!(content[0].inline_boxes.len(), 1);
    let inline_box = &content[0].inline_boxes[0];
    assert_eq!(inline_box.size, LogicalSize::new(px(24.0), px(24.0)));
    assert!(matches!(
        inline_box.paint,
        Some(InlineBoxPaint::Quad { .. })
    ));
    assert_eq!(node_count(&app, node.root()), 2);
}

#[test]
fn images_that_are_not_file_uris_fall_back_to_their_alt_text() {
    let (app, node) = mount("![alt](https://example.com/a.png)");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "alt");
    assert!(content[0].inline_boxes.is_empty());

    let (app, node) = mount("![](https://example.com/a.png)");
    assert!(texts(&app, node.root()).is_empty());
}

#[test]
fn the_release_notes_budget_is_seventy_nodes_with_thirty_four_spacers() {
    for source in [EN_RELEASE_NOTES, ZH_RELEASE_NOTES] {
        let (app, node) = mount(source);
        assert_eq!(count(&app, "MARKDOWN_SPACER"), 34);
        assert_eq!(node_count(&app, node.root()), 70);
        assert_eq!(texts(&app, node.root()).len(), 35);
    }
}

#[test]
fn switching_the_appearance_rebuilds_the_markdown_with_the_new_theme() {
    let mut app = TestApp::new();
    let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    let mut node = MarkdownNode::mount(
        app.app_mut(),
        window,
        root,
        theme.clone(),
        "# Title\n\n正文",
        EmbeddedAssets,
    )
    .expect("the markdown mounts");
    let _ = app.drain().expect("first frame");

    assert_eq!(
        resolved(first_text_color(&app, node.root())),
        resolved(Theme::of(Appearance::Light).role(Role::FgDefault))
    );
    assert_ne!(
        resolved(Theme::of(Appearance::Light).role(Role::FgDefault)),
        resolved(Theme::of(Appearance::Dark).role(Role::FgDefault)),
        "the two appearances must resolve to different foreground colors"
    );
    assert!(
        !node.sync(app.app_mut()),
        "an unchanged appearance is a no-op"
    );

    assert!(theme.set_appearance(app.app_mut(), Appearance::Dark));
    assert!(node.sync(app.app_mut()), "a changed appearance rebuilds");
    let _ = app.drain().expect("the rebuilt frame");
    assert_eq!(
        resolved(first_text_color(&app, node.root())),
        resolved(Theme::of(Appearance::Dark).role(Role::FgDefault))
    );

    assert!(node.release(app.app_mut()));
    assert!(!node.release(app.app_mut()), "a second release is a no-op");
    assert!(
        !node.sync(app.app_mut()),
        "a released markdown does not sync"
    );
}

#[test]
fn the_markdown_subtree_holds_no_signal_dependency_edges() {
    let (mut app, node) = mount("# Title\n\n**bold**\n\n```rust\nlet x = 1;\n```");
    let window = app.window_id();
    for child in children(&app, node.root()) {
        let removed = app
            .app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .remove(child)
            .expect("remove the markdown subtree");
        assert!(removed.is_empty(), "the markdown subtree holds no bind_*");
    }
}

fn first_text_color(app: &TestApp, root: NodeId) -> ui_core::paint::color::Color {
    let tree = tree_of(app);
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let entry = tree.get(node).expect("node alive");
        if let NodeKind::Text(content) = entry.kind() {
            return content.style.color;
        }
        stack.extend(entry.children().iter().rev().copied());
    }
    panic!("the markdown subtree has no text node");
}

fn resolved(color: ui_core::paint::color::Color) -> [f32; 4] {
    ColorResolver.resolve(color).components
}

#[test]
fn container_blocks_do_not_add_spacers_inside_themselves() {
    for (source, expected) in [
        ("- a\n- b\n\np\n", 1),
        ("- a\n\np\n", 1),
        ("- a\n- b\n\n\np\n", 2),
        ("1. x\n\np\n", 1),
        ("\n\na\n", 0),
    ] {
        let (app, _node) = mount(source);
        assert_eq!(count(&app, "MARKDOWN_SPACER"), expected, "{source:?}");
    }
}

#[test]
fn a_table_is_capped_at_twenty_columns() {
    let cells = |prefix: &str| {
        (0..21)
            .map(|index| format!("{prefix}{index}"))
            .collect::<Vec<String>>()
            .join(" | ")
    };
    let source = format!(
        "| {} |\n| {} |\n| {} |\n",
        cells("h"),
        vec!["---"; 21].join(" | "),
        cells("c")
    );
    let (app, node) = mount(&source);
    assert_eq!(count(&app, "MARKDOWN_TABLE_CELL"), 40);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 40);
    assert!(
        content.iter().any(|text| text.text == "h19 h20"),
        "the columns beyond the cap are joined into the last column"
    );
}

#[test]
fn rows_beyond_the_table_cap_become_one_paragraph_after_the_table() {
    let mut source = String::from("| a |\n|---|\n");
    for index in 0..201 {
        source.push_str(&format!("| {index} |\n"));
    }
    let (app, node) = mount(&source);
    assert_eq!(count(&app, "MARKDOWN_TABLE_ROW"), 201);
    let blocks = children(&app, node.root());
    assert_eq!(blocks.len(), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.last().expect("overflow text").text, "200");
}

#[test]
fn block_nesting_beyond_the_cap_flattens_into_one_text_node() {
    let source = format!("{}x", "> ".repeat(70));
    let (app, node) = mount(&source);
    assert_eq!(count(&app, "MARKDOWN_QUOTE"), 64);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");
}

#[test]
fn inline_nesting_beyond_the_cap_drops_the_text() {
    let source = format!("{}x{}", "**".repeat(80), "**".repeat(80));
    let (app, node) = mount(&source);
    assert_eq!(node_count(&app, node.root()), 1);
}
fn nodes_with_id(app: &TestApp, root: NodeId, id: &str) -> Vec<NodeId> {
    let tree = tree_of(app);
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if tree.debug_id(node) == Some(id) {
            found.push(node);
        }
        stack.extend(
            tree.get(node)
                .expect("node alive")
                .children()
                .iter()
                .rev()
                .copied(),
        );
    }
    found
}

fn glyph_run(app: &TestApp, node: NodeId) -> (LogicalPx, LogicalPx) {
    let primitive = app
        .rendered()
        .expect("frame")
        .primitives
        .iter()
        .find(|primitive| {
            primitive.node == node && matches!(primitive.kind, PrimitiveKind::GlyphRun)
        })
        .expect("glyph run");
    (primitive.bounds.origin.x, primitive.bounds.size.width)
}

fn measured_width(app: &mut TestApp, text: &str, header: bool) -> LogicalPx {
    let style = style();
    let content = TextContent::new(
        SmolStr::new(text),
        TextStyle::new(style.body_size)
            .with_color(style.foreground)
            .with_weight(if header { 700 } else { 400 })
            .with_line_height(style.line_height)
            .with_alignment(TextAlign::Left)
            .with_wrap(TextWrap::BreakWord),
    );
    app.app_mut().measure_text(&content, None).width
}

fn cell_widths(app: &TestApp, root: NodeId) -> Vec<LogicalPx> {
    nodes_with_id(app, root, "MARKDOWN_TABLE_CELL")
        .into_iter()
        .map(|cell| bounds(app, cell).size.width)
        .collect()
}

#[test]
fn an_html_paragraph_pair_renders_one_aligned_text() {
    let (app, node) = mount("<p align=\"center\">hello</p>");
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "hello");
    assert_eq!(content[0].style.alignment, TextAlign::Center);
}

#[test]
fn an_html_paragraph_pair_parses_inline_markdown_inside_itself() {
    let source = "<p align=\"center\">**bold** [x](u) ![a](file://icons/phosphoricons/arrow-up.svg) <kbd>K</kbd></p>";
    let (app, node) = mount(source);
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "bold x  K");
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    let runs = content[0].runs();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].range, 0..4);
    assert_eq!(runs[0].style.weight, 700);
    assert_eq!(runs[1].range, 5..6);
    assert_eq!(runs[1].style.color, style().link);
    assert!(runs[1].style.decorations().underline.is_some());
    assert_eq!(runs[2].range, 8..9);
    assert!(runs[2].style.decorations().underline.is_some());
    assert!(runs[2].style.background().is_some());
    assert_eq!(content[0].inline_boxes.len(), 1);
    assert_eq!(
        content[0].inline_boxes[0].size,
        LogicalSize::new(px(16.0), px(16.0))
    );

    let production = "<p align=\"center\">This widget needs the metadata of a supported market participant to be loaded first.\nLoad the metadata of at least one supported market participant from the <kbd>![](file://icons/phosphoricons/bank.svg)</kbd> button in the top left corner.\nThe market participants this widget supports are:\n%{markets}</p>";
    let (app, node) = mount(production);
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(
        content[0].text,
        "This widget needs the metadata of a supported market participant to be loaded first.\nLoad the metadata of at least one supported market participant from the  button in the top left corner.\nThe market participants this widget supports are:\n%{markets}"
    );
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[0].inline_boxes.len(), 1);
    assert!(
        content[0].runs().is_empty(),
        "an image wrapped in kbd cuts no text run"
    );
}

#[test]
fn an_html_paragraph_without_an_attribute_is_left_aligned() {
    let (app, node) = mount("<p>hello</p>");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "hello");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
}

#[test]
fn html_paragraph_alignment_reaches_the_first_glyph() {
    for (source, expected) in [
        ("<p align=\"left\">wide text</p>", TextAlign::Left),
        ("<p align=\"center\">wide text</p>", TextAlign::Center),
        ("<p align=\"right\">wide text</p>", TextAlign::Right),
    ] {
        let (app, node) = mount(source);
        let content = texts(&app, node.root());
        assert_eq!(content[0].style.alignment, expected, "{source}");
    }

    let (left_app, left_node) = mount("<p align=\"left\">wide text</p>");
    let (center_app, center_node) = mount("<p align=\"center\">wide text</p>");
    let (right_app, right_node) = mount("<p align=\"right\">wide text</p>");
    let left_text = children(&left_app, left_node.root())[0];
    let center_text = children(&center_app, center_node.root())[0];
    let right_text = children(&right_app, right_node.root())[0];
    let (left_x, _left_width) = glyph_run(&left_app, left_text);
    let (center_x, center_width) = glyph_run(&center_app, center_text);
    let (right_x, right_width) = glyph_run(&right_app, right_text);
    assert!(left_x < px(2.0));
    assert!((center_x + center_width / 2.0 - px(400.0)).abs() < px(2.0));
    assert!((right_x + right_width - px(800.0)).abs() < px(2.0));
}

#[test]
fn several_html_tag_pairs_in_one_block_become_separate_paragraphs() {
    let (app, node) = mount("<p align=\"center\">a</p><p align=\"right\">b</p>");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 0);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "a");
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].text, "b");
    assert_eq!(content[1].style.alignment, TextAlign::Right);

    let (app, node) = mount("<p align=\"center\">a</p><p align=\"right\">b\n\nc");
    let content = texts(&app, node.root());
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 1);
    assert_eq!(content.len(), 3);
    assert_eq!(content[2].text, "c");
    assert_eq!(content[2].style.alignment, TextAlign::Right);

    let (app, node) = mount("<p align=\"center\">a</p></p>\n\nb");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].text, "b");
    assert_eq!(content[1].style.alignment, TextAlign::Left);
}

#[test]
fn a_closing_html_tag_in_a_paragraph_ends_the_alignment_window_after_it() {
    let (app, node) = mount("<p align=\"center\">a\n\nb</p>");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "a");
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].text, "b");
    assert_eq!(content[1].style.alignment, TextAlign::Center);

    let (app, node) = mount("<p align=\"center\">a\n\nb</p>\n\nc");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 3);
    assert_eq!(content[2].text, "c");
    assert_eq!(
        content[2].style.alignment,
        TextAlign::Left,
        "the inline closing tag pops the window after its own paragraph"
    );
}

#[test]
fn an_alignment_window_spans_headings_and_later_paragraphs() {
    let (app, node) = mount("<p align=\"center\">\n\n# Heading\n\npara\n\n</p>");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 3);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "Heading");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
    assert_eq!(content[1].text, "para");
    assert_eq!(content[1].style.alignment, TextAlign::Center);
}

#[test]
fn blank_lines_inside_an_alignment_window_become_spacers() {
    let (app, node) = mount("<p align=\"center\">a\n\n\n\nb</p>");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 3);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].style.alignment, TextAlign::Center);
}

#[test]
fn nested_alignment_windows_pop_once_per_closing_tag() {
    let (app, node) = mount("<p align=\"center\"><p align=\"right\">x\n\ny\n\n</p>\n\nz");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 3);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 3);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[0].style.alignment, TextAlign::Right);
    assert_eq!(content[1].text, "y");
    assert_eq!(content[1].style.alignment, TextAlign::Right);
    assert_eq!(content[2].text, "z");
    assert_eq!(
        content[2].style.alignment,
        TextAlign::Center,
        "one block-level closing tag pops exactly one window"
    );

    let (app, node) = mount("<p align=\"center\"><p align=\"right\">x");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].style.alignment, TextAlign::Right);
}

#[test]
fn an_unclosed_alignment_tag_keeps_its_alignment_until_the_end() {
    let (app, node) = mount("<p align=\"center\">hello\n\nworld");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "hello");
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].text, "world");
    assert_eq!(content[1].style.alignment, TextAlign::Center);
}

#[test]
fn a_stray_closing_tag_is_dropped() {
    let (app, node) = mount("</p>\n\nx");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
}

#[test]
fn a_literal_closing_tag_in_a_paragraph_closes_the_window_early() {
    let (app, node) = mount("<p align=\"center\">a\n\nb</p>\n\nc");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 3);
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].style.alignment, TextAlign::Center);
    assert_eq!(content[2].text, "c");
    assert_eq!(content[2].style.alignment, TextAlign::Left);
}

#[test]
fn an_html_division_parses_its_content_as_markdown() {
    let (app, node) = mount("<div>\n**bold** text\n</div>");
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "bold text");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
    assert_eq!(content[0].runs().len(), 1);
    assert_eq!(content[0].runs()[0].style.weight, 700);

    let (app, node) = mount("<div>\n<p align=\"center\">x</p>\n</div>");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[0].style.alignment, TextAlign::Center);
}

#[test]
fn center_and_section_scope_alignment_without_adding_nodes() {
    let (app, node) = mount("<center>x</center>");
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content[0].style.alignment, TextAlign::Center);

    let (app, node) = mount("<section>**b** y</section>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "b y");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
    assert_eq!(content[0].runs().len(), 1);
}

#[test]
fn html_headings_and_horizontal_rules_mount_block_nodes() {
    let body = style().body_size;
    for (level, scale) in [
        (1, 1.75),
        (2, 1.5),
        (3, 1.3),
        (4, 1.15),
        (5, 1.05),
        (6, 1.0),
    ] {
        let source = format!("<h{level}>Title</h{level}>");
        let (app, node) = mount(&source);
        let content = texts(&app, node.root());
        assert_eq!(content.len(), 1, "{source}");
        assert!(
            (content[0].style.size - body * scale).abs() < px(0.01),
            "{source}"
        );
        assert_eq!(content[0].style.weight, 700, "{source}");
        assert_eq!(content[0].style.alignment, TextAlign::Left, "{source}");
    }

    let (app, node) = mount("<h3>*b*</h3>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].runs().len(), 1);
    assert_eq!(
        content[0].runs()[0].style.slant,
        ui_core::text::TextSlant::Italic
    );

    for source in ["<hr>", "<hr/>", "<hr />"] {
        let (app, node) = mount(source);
        assert_eq!(count(&app, "MARKDOWN_RULE"), 1, "{source}");
        assert_eq!(node_count(&app, node.root()), 2, "{source}");
        assert!(
            (bounds(&app, node_of(&app, "MARKDOWN_RULE")).size.height - px(1.0)).abs() < px(0.01)
        );
    }

    let (app, node) = mount("<p align=\"center\">a</p>\n<hr>\n<p align=\"right\">b</p>");
    assert_eq!(count(&app, "MARKDOWN_RULE"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].text, "b");
    assert_eq!(content[1].style.alignment, TextAlign::Right);
}

#[test]
fn non_exact_html_paragraphs_keep_their_text_and_lose_the_alignment() {
    for (source, expected) in [
        ("<p align='center'>x</p>", "x"),
        ("<p align=center>x</p>", "x"),
        ("<p ALIGN=\"center\">x</p>", "x"),
        ("<P align=\"center\">x</P>", "x"),
        ("<p align = \"center\">x</p>", "x"),
        ("<p class=\"x\">x</p>", "x"),
        ("<p align=\"center\" class=\"x\">x</p>", "x"),
        ("<p align=\"center\" align=\"left\">x</p>", "x"),
        ("<p align=\"justify\">x</p>", "x"),
        ("<p >x</p>", "x"),
        ("<div >x</div>", "x"),
        ("<center >x</center>", "x"),
        ("<h2 >x</h2>", "x"),
        (
            "<div>\n<palign=\"center\">x</p>\n</div>",
            "<palign=\"center\">x",
        ),
    ] {
        let (app, node) = mount(source);
        let content = texts(&app, node.root());
        assert_eq!(content.len(), 1, "{source}");
        assert_eq!(content[0].text, expected, "{source}");
        assert_eq!(content[0].style.alignment, TextAlign::Left, "{source}");
    }

    let (app, node) = mount("<p align=\"center\">x</p >");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");
}

#[test]
fn html_comments_never_open_an_alignment_window() {
    for source in [
        "<!-- <p align=\"center\">hidden</p> -->",
        "<!--\n<p align=\"center\">hidden</p>\n-->",
    ] {
        let (app, node) = mount(source);
        assert_eq!(node_count(&app, node.root()), 1, "{source}");
        assert!(texts(&app, node.root()).is_empty(), "{source}");
    }

    for comment in ["<!-->", "<!--->"] {
        let source = format!("<div>\n{comment}\n<p align=\"center\">x</p>\n</div>");
        let (app, node) = mount(&source);
        let content = texts(&app, node.root());
        assert_eq!(content.len(), 1, "{comment}");
        assert_eq!(content[0].text, "x");
        assert_eq!(content[0].style.alignment, TextAlign::Center);
    }

    let (app, node) = mount(
        "<div>\n<!-- > <p align=\"center\">hidden</p> -->\n<p align=\"center\">x</p>\n</div>",
    );
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");

    let (app, node) = mount("<div>\n<!-- don't -->\n<p align=\"center\">x</p>\n</div>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].style.alignment, TextAlign::Center);

    let (app, node) = mount("<p align=\"center\">a<!-- </p> -->b</p>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "ab");
    assert_eq!(content[0].style.alignment, TextAlign::Center);
}

#[test]
fn crlf_and_carriage_return_line_endings_count_blank_lines() {
    let (app, node) = mount("<p align=\"center\">a\r\n\r\nb</p>");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].style.alignment, TextAlign::Center);

    let (app, _node) = mount("a\r\rb");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 1);

    let (app, node) = mount("<p align=\"center\">a\r\rb</p>");
    assert_eq!(
        count(&app, "MARKDOWN_SPACER"),
        0,
        "a fragment never inserts spacers"
    );
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].style.alignment, TextAlign::Center);
}

#[test]
fn tag_block_types_one_to_five_are_ignored_as_a_whole() {
    for source in [
        "<!-- <p align=\"center\">hidden</p> -->",
        "<pre>\n<p align=\"center\">hidden</p>\n</pre>",
        "<PRE>\n<p align=\"center\">hidden</p>\n</PRE>",
        "<script>\n<p align=\"center\">hidden</p>\n</script>",
        "<style>\n<p align=\"center\">hidden</p>\n</style>",
        "<textarea>\n<p align=\"center\">hidden</p>\n</textarea>",
        "<?php echo 1; ?>",
        "<![CDATA[ <p align=\"center\">hidden</p> ]]>",
        "<!DOCTYPE html>",
    ] {
        let (app, node) = mount(source);
        assert_eq!(node_count(&app, node.root()), 1, "{source}");
        assert!(texts(&app, node.root()).is_empty(), "{source}");
    }

    let (app, node) = mount("<p align=\"center\">x</p>\n\n<pre>hidden</pre>");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");
}

#[test]
fn inline_bold_italic_and_crossed_tags_pair_by_name() {
    let (app, node) = mount("a <b>**x**</b> b");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a x b");
    assert_eq!(content[0].runs().len(), 1);
    assert_eq!(content[0].runs()[0].range, 2..3);
    assert_eq!(content[0].runs()[0].style.weight, 700);

    let (app, node) = mount("<strong>s</strong><em>e</em>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "se");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].range, 0..1);
    assert_eq!(runs[0].style.weight, 700);
    assert_eq!(runs[1].range, 1..2);
    assert_eq!(runs[1].style.slant, ui_core::text::TextSlant::Italic);

    let (app, node) = mount("<b><i></b></i>x");
    assert_eq!(texts(&app, node.root())[0].text, "x");
    assert!(texts(&app, node.root())[0].runs().is_empty());

    let (app, node) = mount("</b>x");
    assert_eq!(texts(&app, node.root())[0].text, "x");
    assert!(texts(&app, node.root())[0].runs().is_empty());

    let (app, node) = mount("<b><b>x</b></b>");
    assert_eq!(texts(&app, node.root())[0].runs().len(), 1);
    assert_eq!(texts(&app, node.root())[0].runs()[0].style.weight, 700);

    let (app, node) = mount("<b>a<i>b</b>c</i>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "abc");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].range, 0..1);
    assert_eq!(runs[0].style.weight, 700);
    assert_eq!(runs[1].range, 1..2);
    assert_eq!(runs[1].style.weight, 700);
    assert_eq!(runs[1].style.slant, ui_core::text::TextSlant::Italic);
    assert_eq!(runs[2].range, 2..3);
    assert_eq!(runs[2].style.weight, 400);
    assert_eq!(runs[2].style.slant, ui_core::text::TextSlant::Italic);
}

#[test]
fn inline_del_code_span_and_break_tags_map_to_runs() {
    let (app, node) = mount("<del>d</del><s>s</s>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "ds");
    assert_eq!(content[0].runs().len(), 1);
    assert!(
        content[0].runs()[0]
            .style
            .decorations()
            .strikethrough
            .is_some()
    );

    let (app, node) = mount("<code>x</code>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].runs().len(), 1);
    let run = &content[0].runs()[0];
    assert_eq!(run.style.family().map(SmolStr::as_str), Some(MONO_FAMILY));
    assert_eq!(run.style.color, style().code_foreground);
    assert!(run.style.background().is_some());

    let (app, node) = mount("<span>x</span>");
    assert_eq!(texts(&app, node.root())[0].text, "x");
    assert!(texts(&app, node.root())[0].runs().is_empty());

    let (app, node) = mount("<code>a<br>b</code>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a\nb");
    assert_eq!(content[0].runs().len(), 1);
    assert_eq!(content[0].runs()[0].range, 0..3);
    assert!(bounds(&app, children(&app, node.root())[0]).size.height > style().line_height() * 1.5);
}

#[test]
fn inline_anchor_and_image_tags_map_to_marks_and_boxes() {
    let (app, node) = mount("<a href=\"https://example.com\">link</a>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "link");
    let run = &content[0].runs()[0];
    assert_eq!(run.style.color, style().link);
    assert!(run.style.decorations().underline.is_some());

    let (app, node) = mount("a <img src=\"file://icons/phosphoricons/bank.svg\"> b");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a  b");
    assert_eq!(content[0].inline_boxes.len(), 1);
    assert_eq!(
        content[0].inline_boxes[0].size,
        LogicalSize::new(px(16.0), px(16.0))
    );

    let (app, node) = mount("a <img src=\"https://example.com/a.png\" alt=\"alt\"> b");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a alt b");
    assert!(content[0].inline_boxes.is_empty());

    let (app, node) = mount("a <img src=\"https://example.com/a.png\"> b");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a  b");
    assert!(content[0].inline_boxes.is_empty());

    let (app, node) = mount("text <p align=\"center\">**bold**</p>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "text bold");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
    assert_eq!(content[0].runs().len(), 1);
    assert_eq!(content[0].runs()[0].style.weight, 700);
}

#[test]
fn kbd_is_exact_and_may_wrap_an_image() {
    let (app, node) = mount("a <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> b");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a  b");
    assert_eq!(content[0].inline_boxes.len(), 1);

    let (app, node) = mount("a <kbd>Ctrl</kbd> b");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a Ctrl b");
    let run = &content[0].runs()[0];
    assert_eq!(run.range, 2..6);
    assert_eq!(run.style.family().map(SmolStr::as_str), Some(MONO_FAMILY));
    assert!(run.style.decorations().underline.is_some());
    assert!(run.style.background().is_some());

    for source in ["<KBD>x</KBD>", "<kbd class=\"k\">x</kbd>", "<kbd >x</kbd>"] {
        let (app, node) = mount(source);
        let content = texts(&app, node.root());
        assert_eq!(content[0].text, "x", "{source}");
        assert!(content[0].runs().is_empty(), "{source}");
    }
}

#[test]
fn html_blocks_inside_containers_degrade_to_one_plain_paragraph() {
    let (app, node) = mount("> <div>**x**</div>");
    assert_eq!(count(&app, "MARKDOWN_QUOTE"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "**x**");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
    assert!(content[0].runs().is_empty());

    let (app, node) = mount("- <p align=\"center\">x</p>");
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 0);
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT_TIGHT"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert!(content.iter().any(|text| text.text == "•"));
    let paragraph = content
        .iter()
        .find(|text| text.text == "x")
        .expect("the item text");
    assert_eq!(paragraph.style.alignment, TextAlign::Left);

    let (app, node) = mount("<p align=\"center\">x</p>\n<!-- 中");
    assert_eq!(texts(&app, node.root())[0].text, "x");

    let (app, node) = mount("<div>\n<!-- 中");
    assert_eq!(node_count(&app, node.root()), 1);
}

#[test]
fn table_columns_take_the_four_alignment_kinds() {
    let (app, node) = mount("| a | b | c | d |\n|:--|:-:|--:|---|\n| 1 | 2 | 3 | 4 |");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 8);
    let aligns = content
        .iter()
        .map(|text| text.style.alignment)
        .collect::<Vec<TextAlign>>();
    assert_eq!(
        aligns,
        vec![
            TextAlign::Left,
            TextAlign::Center,
            TextAlign::Right,
            TextAlign::Left,
            TextAlign::Left,
            TextAlign::Center,
            TextAlign::Right,
            TextAlign::Left,
        ]
    );
}

#[test]
fn empty_table_cells_mount_one_node_without_text() {
    let (app, node) = mount("| a | b |\n|---|---|\n| 1 | |");
    assert_eq!(count(&app, "MARKDOWN_TABLE_CELL"), 4);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 3);
    assert_eq!(content[2].text, "1");
}

#[test]
fn escaped_pipes_keep_their_text_in_cells_and_code_spans() {
    let (app, node) = mount("| a\\|b | c |\n|---|---|\n| 1 | 2 |");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a|b");

    let (app, node) = mount("| `a\\|b` | c |\n|---|---|\n| 1 | 2 |");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a|b");
    assert_eq!(content[0].runs().len(), 1);
    assert_eq!(
        content[0].runs()[0].style.family().map(SmolStr::as_str),
        Some(MONO_FAMILY)
    );
}

#[test]
fn table_cells_carry_inline_links_and_image_boxes() {
    let source = "| a | b |\n|---|---|\n| [x](u) | ![i](file://icons/phosphoricons/arrow-up.svg) |";
    let (app, node) = mount(source);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 4);
    assert_eq!(content[2].text, "x");
    assert_eq!(content[2].runs().len(), 1);
    assert_eq!(content[2].runs()[0].style.color, style().link);
    assert_eq!(content[3].text, "\u{200B}");
    assert_eq!(content[3].inline_boxes.len(), 1);
}

#[test]
fn a_short_table_row_is_padded_up_to_the_header() {
    let (app, node) = mount("| a | b | c |\n|---|---|---|\n| 1 |");
    assert_eq!(count(&app, "MARKDOWN_TABLE_CELL"), 6);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 4);
    assert_eq!(
        content
            .iter()
            .map(|text| text.text.as_str())
            .collect::<Vec<&str>>(),
        vec!["a", "b", "c", "1"]
    );
}

#[test]
fn a_table_with_only_a_header_still_has_its_rule() {
    let (app, node) = mount("| only |\n|---|\n");
    assert_eq!(count(&app, "MARKDOWN_TABLE"), 1);
    assert_eq!(count(&app, "MARKDOWN_TABLE_ROW"), 1);
    assert_eq!(count(&app, "MARKDOWN_TABLE_RULE"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
}

#[test]
fn the_header_row_is_bold_and_the_body_is_not() {
    let (app, node) = mount("| **h** | x |\n|---|---|\n| 1 | 2 |");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 4);
    assert_eq!(content[0].style.weight, 700);
    assert_eq!(content[1].style.weight, 700);
    assert_eq!(content[2].style.weight, 400);
    assert_eq!(content[3].style.weight, 400);
}

#[test]
fn a_table_inside_a_list_item_measures_its_columns() {
    let (app, node) = mount("- | a | b |\n  |---|---|\n  | 1 | 2 |");
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 0);
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT_TIGHT"), 1);
    assert_eq!(count(&app, "MARKDOWN_TABLE"), 1);
    assert_eq!(count(&app, "MARKDOWN_TABLE_CELL"), 4);
    let widths = cell_widths(&app, node.root());
    assert!((widths[0] - widths[2]).abs() < px(0.01));
    assert!((widths[1] - widths[3]).abs() < px(0.01));
}

#[test]
fn same_column_cells_share_the_measured_natural_width() {
    let source = "| short | a very long piece of cell text here |\n|---|---|\n| s | t |";
    let (mut app, node) = mount(source);
    let widths = cell_widths(&app, node.root());
    assert_eq!(widths.len(), 4);
    assert!((widths[0] - widths[2]).abs() < px(0.01));
    assert!((widths[1] - widths[3]).abs() < px(0.01));
    let short = measured_width(&mut app, "short", true);
    let long = measured_width(&mut app, "a very long piece of cell text here", true);
    assert!(
        (widths[0] - (short + px(8.0))).abs() < px(1.0),
        "the column width is the natural width plus two 4px paddings"
    );
    assert!((widths[1] - (long + px(8.0))).abs() < px(1.0));
}

#[test]
fn a_wider_cell_gets_a_wider_column() {
    let source = "| short | a very long piece of cell text here |\n|---|---|\n| s | t |";
    let (mut app, node) = mount(source);
    let widths = cell_widths(&app, node.root());
    assert!(widths[1] > widths[0]);
    let short = measured_width(&mut app, "short", true);
    let long = measured_width(&mut app, "a very long piece of cell text here", true);
    let ratio = widths[1] / widths[0];
    let expected = (long + px(8.0)) / (short + px(8.0));
    assert!(
        ((ratio - expected) / expected).abs() < 0.05,
        "the rendered width ratio {ratio} tracks the natural width ratio {expected}"
    );

    let left = "x".repeat(100);
    let right = "y".repeat(110);
    let source = format!("| {left} | {right} |\n|---|---|\n| a | b |");
    let (mut app, node) = mount(&source);
    let widths = cell_widths(&app, node.root());
    let left_natural = measured_width(&mut app, &left, true) + px(8.0);
    let right_natural = measured_width(&mut app, &right, true) + px(8.0);
    assert!(
        left_natural + right_natural > px(800.0),
        "the row must be shrunk"
    );
    assert!(widths[0] + widths[1] <= px(800.5), "the row fits the table");
    assert!(widths[1] > widths[0]);
    let ratio = widths[1] / widths[0];
    let expected = right_natural / left_natural;
    assert!(
        ((ratio - expected) / expected).abs() < 0.05,
        "a shrunk table keeps the natural width ratio: {ratio} vs {expected}"
    );
}

#[test]
fn a_table_inside_html_container_content_measures_its_columns() {
    let source = "<div>\n| a | b |\n|---|---|\n| 1 | long cell text |\n</div>";
    let (mut app, node) = mount(source);
    assert_eq!(count(&app, "MARKDOWN_TABLE"), 1);
    let widths = cell_widths(&app, node.root());
    assert_eq!(widths.len(), 4);
    assert!((widths[0] - widths[2]).abs() < px(0.01));
    assert!((widths[1] - widths[3]).abs() < px(0.01));
    let long = measured_width(&mut app, "long cell text", false);
    assert!((widths[1] - (long + px(8.0))).abs() < px(1.0));
}

#[test]
fn a_table_falls_back_to_equal_columns_without_a_ready_font_stack() {
    let mut app = TestApp::new();
    let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
    app.app_mut()
        .set_font_stack(FontStack::new(Vec::<SmolStr>::new()));
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    let node = MarkdownNode::mount(
        app.app_mut(),
        window,
        root,
        theme,
        "| a | longer |\n|---|---|\n| 1 | 2 |",
        EmbeddedAssets,
    )
    .expect("the markdown mounts");
    let _ = app.drain().expect("first frame");
    let widths = cell_widths(&app, node.root());
    assert_eq!(widths.len(), 4);
    assert!((widths[0] - widths[1]).abs() < px(0.01));
    assert!((widths[2] - widths[3]).abs() < px(0.01));
    assert_eq!(texts(&app, node.root()).len(), 4);
}

#[test]
fn tight_task_items_mount_a_checkbox_marker_column() {
    let (app, node) = mount("- [ ] one\n- [x] two");
    assert_eq!(count(&app, "MARKDOWN_LIST_ITEM"), 2);
    assert_eq!(count(&app, "MARKDOWN_LIST_MARKER"), 2);
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 0);
    let items = nodes_with_id(&app, node.root(), "MARKDOWN_LIST_ITEM");
    assert_eq!(node_count(&app, items[0]), 3);
    assert_eq!(node_count(&app, items[1]), 4);
    let markers = nodes_with_id(&app, node.root(), "MARKDOWN_LIST_MARKER");
    for marker in &markers {
        assert_eq!(
            bounds(&app, *marker).size,
            LogicalSize::new(px(14.0), px(14.0))
        );
    }
    let content = texts(&app, node.root());
    assert_eq!(content.iter().filter(|text| text.text == "✓").count(), 1);
    assert_eq!(
        content
            .iter()
            .filter(|text| text.text == "one" || text.text == "two")
            .count(),
        2
    );
}

#[test]
fn loose_task_items_keep_a_content_column() {
    let (app, node) = mount("- [ ] one\n\n- [x] two");
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 2);
    let items = nodes_with_id(&app, node.root(), "MARKDOWN_LIST_ITEM");
    assert_eq!(node_count(&app, items[0]), 4);
    assert_eq!(node_count(&app, items[1]), 5);
}

#[test]
fn the_check_mark_only_shows_for_checked_items() {
    let (app, node) = mount("- [ ] a\n- [x] b\n- [X] c");
    let content = texts(&app, node.root());
    assert_eq!(content.iter().filter(|text| text.text == "✓").count(), 2);
    let markers = nodes_with_id(&app, node.root(), "MARKDOWN_LIST_MARKER");
    let quads = &app.rendered().expect("frame").quads;
    let fills = markers
        .iter()
        .map(|marker| {
            quads
                .iter()
                .find(|quad| quad.node == *marker)
                .expect("checkbox quad")
                .fill
                .components
        })
        .collect::<Vec<[f32; 4]>>();
    assert_eq!(fills[0], [0.0, 0.0, 0.0, 0.0]);
    assert_eq!(fills[1], resolved(style().task_fill));
    assert_eq!(fills[2], resolved(style().task_fill));
    let borders = markers
        .iter()
        .map(|marker| {
            quads
                .iter()
                .find(|quad| quad.node == *marker)
                .expect("checkbox quad")
                .border
                .expect("checkbox border")
        })
        .collect::<Vec<_>>();
    for (width, color) in borders {
        assert_eq!(width, px(1.5));
        assert_eq!(color.components, resolved(style().task_border));
    }
}

#[test]
fn task_markers_do_not_reach_the_text_content() {
    let (app, node) = mount("- [ ] one\n- [x] two");
    let content = texts(&app, node.root());
    assert!(!content.iter().any(|text| text.text.contains("[ ]")));
    assert!(!content.iter().any(|text| text.text.contains("[x]")));
}

#[test]
fn task_lists_nest() {
    let (app, node) = mount("- [ ] one\n  - [x] two");
    assert_eq!(count(&app, "MARKDOWN_LIST"), 2);
    assert_eq!(count(&app, "MARKDOWN_LIST_MARKER"), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.iter().filter(|text| text.text == "✓").count(), 1);
}

#[test]
fn task_lists_mix_with_plain_items() {
    let (app, node) = mount("- [ ] a\n- plain\n- [x] b");
    assert_eq!(count(&app, "MARKDOWN_LIST_ITEM"), 3);
    assert_eq!(count(&app, "MARKDOWN_LIST_MARKER"), 3);
    let content = texts(&app, node.root());
    assert_eq!(content.iter().filter(|text| text.text == "•").count(), 1);
    assert_eq!(content.iter().filter(|text| text.text == "✓").count(), 1);
}

#[test]
fn an_empty_task_item_still_shows_its_checkbox() {
    let (app, node) = mount("- [ ] \n- [x] ");
    assert_eq!(count(&app, "MARKDOWN_LIST_ITEM"), 2);
    let markers = nodes_with_id(&app, node.root(), "MARKDOWN_LIST_MARKER");
    assert_eq!(markers.len(), 2);
    for marker in &markers {
        assert_eq!(
            bounds(&app, *marker).size,
            LogicalSize::new(px(14.0), px(14.0))
        );
    }
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "✓");
}

#[test]
fn ordered_task_items_use_the_checkbox_instead_of_the_number() {
    let (app, node) = mount("1. [ ] one\n2. [x] two");
    let content = texts(&app, node.root());
    assert!(!content.iter().any(|text| text.text.ends_with('.')));
    assert_eq!(content.iter().filter(|text| text.text == "✓").count(), 1);
}

#[test]
fn a_literal_closing_tag_outside_any_window_changes_nothing() {
    let (app, node) = mount("a </p> b");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a  b");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
}

#[test]
fn html_nesting_beyond_the_parse_budget_degrades_to_plain_text() {
    let source = format!("{}deep{}", "<div>".repeat(20), "</div>".repeat(20));
    let (app, node) = mount(&source);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "deep");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
}

#[test]
fn an_html_block_beyond_the_tag_cap_degrades_to_plain_text() {
    let source = "<p>x</p>".repeat(5000);
    let (app, node) = mount(&source);
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert!(!content[0].text.contains('<'));
    assert!(content[0].text.contains('x'));
}

#[test]
fn a_degraded_fragment_shows_no_markdown_markers() {
    let source = format!("<center>**{}**</center>", "a".repeat(70_000));
    let (app, node) = mount(&source);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text.len(), 70_000);
    assert!(content[0].text.bytes().all(|byte| byte == b'a'));
}

#[test]
fn html_containers_pair_by_tag_name_and_keep_the_inner_pair() {
    let (app, node) = mount("<div>a<p>x</p>b</div>");
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "axb");
    assert_eq!(content[0].style.alignment, TextAlign::Left);

    let (app, node) = mount("<center>a<p>x</p>b</center>");
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "axb");
    assert_eq!(content[0].style.alignment, TextAlign::Center);

    let (app, node) = mount("<div>a<div>b</div>c</div>");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "abc");
}

#[test]
fn an_html_pair_inside_a_center_keeps_its_own_alignment_and_the_tail_centers() {
    let (app, node) = mount("<center><p align=\"right\">x</p>tail</center>");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[0].style.alignment, TextAlign::Right);
    assert_eq!(content[1].text, "tail");
    assert_eq!(content[1].style.alignment, TextAlign::Center);
}

#[test]
fn an_unpaired_html_heading_keeps_the_body_style() {
    let (app, node) = mount("<h1>abc");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "abc");
    assert!((content[0].style.size - style().body_size).abs() < px(0.01));
    assert_eq!(content[0].style.weight, 400);
}

#[test]
fn an_inline_closing_tag_keeps_both_paragraphs_of_its_window_centered() {
    let (app, node) = mount("<p align=\"center\">a\n\nb</p>");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "a");
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].text, "b");
    assert_eq!(content[1].style.alignment, TextAlign::Center);
}

#[test]
fn both_alignment_window_close_paths_pop_exactly_once() {
    let (app, node) = mount("<p align=\"center\">a</p>\n\nb");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].text, "b");
    assert_eq!(content[1].style.alignment, TextAlign::Left);

    let (app, node) = mount("<p align=\"center\">a\n\nb</p>\n\nc");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 3);
    assert_eq!(content[0].style.alignment, TextAlign::Center);
    assert_eq!(content[1].style.alignment, TextAlign::Center);
    assert_eq!(content[2].text, "c");
    assert_eq!(content[2].style.alignment, TextAlign::Left);
}

#[test]
fn each_inline_closing_tag_pops_one_alignment_window() {
    let (app, node) = mount("<p align=\"center\"><p align=\"right\">\n\nx</p></p>\n\ny");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[0].style.alignment, TextAlign::Right);
    assert_eq!(content[1].text, "y");
    assert_eq!(content[1].style.alignment, TextAlign::Left);
}

#[test]
fn an_inline_closing_tag_in_a_heading_closes_the_alignment_window() {
    let (app, node) = mount("<p align=\"center\">\n\n# H </p>\n\nb");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "H ");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
    assert_eq!(content[1].text, "b");
    assert_eq!(content[1].style.alignment, TextAlign::Left);
}

#[test]
fn an_inline_closing_tag_in_a_table_cell_closes_the_alignment_window() {
    let (app, node) = mount("<p align=\"center\">a\n\n| x | y |\n|---|---|\n| </p> | b |\n\nc");
    let content = texts(&app, node.root());
    assert_eq!(content.last().expect("tail paragraph").text, "c");
    assert_eq!(
        content.last().expect("tail paragraph").style.alignment,
        TextAlign::Left
    );
}

#[test]
fn a_table_cell_flushes_its_inline_content_into_the_cell() {
    let (app, node) = mount("| a <br> b |\n|---|\n| c |\n");
    assert_eq!(count(&app, "MARKDOWN_TABLE_CELL"), 2);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "a \n b");
    assert_eq!(content[1].text, "c");
}

#[test]
fn an_empty_code_block_still_mounts_its_padded_box() {
    let (app, node) = mount("```\n```");
    assert_eq!(node_count(&app, node.root()), 2);
    assert_eq!(count(&app, "MARKDOWN_CODE"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "");
    let code = node_of(&app, "MARKDOWN_CODE");
    let code_bounds = bounds(&app, code);
    assert!((code_bounds.size.width - px(800.0)).abs() < px(0.01));
    assert!(code_bounds.size.height >= px(16.0));
    let quad = app
        .rendered()
        .expect("the first frame renders")
        .quads
        .iter()
        .find(|quad| quad.node == code)
        .expect("the empty code block still paints its background");
    assert_eq!(quad.bounds, code_bounds);

    let (app, node) = mount("x\n\n```\n\n```\n\ny");
    assert_eq!(node_count(&app, node.root()), 6);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 3);
    assert_eq!(content[1].text, "");
}

#[test]
fn overflow_rows_join_every_cell_including_empty_ones() {
    let mut source = String::from("| a | b | c |\n|---|---|---|\n");
    for index in 0..200 {
        source.push_str(&format!("| {index} | x | y |\n"));
    }
    source.push_str("| p | | r |\n");
    let (app, node) = mount(&source);
    let content = texts(&app, node.root());
    assert_eq!(content.last().expect("overflow paragraph").text, "p  r");
}

#[test]
fn empty_list_items_keep_the_body_line_height() {
    let line = style().line_height();
    let (app, node) = mount("- [ ]\n- [x]\n- c\n");
    let items = nodes_with_id(&app, node.root(), "MARKDOWN_LIST_ITEM");
    assert_eq!(items.len(), 3);
    for item in &items {
        assert!((bounds(&app, *item).size.height - line).abs() < px(0.01));
    }
    assert!((bounds(&app, items[2]).origin.y - line * 2.0).abs() < px(0.01));
}

#[test]
fn empty_loose_items_keep_a_content_column() {
    let (app, _node) = mount("- a\n\n-\n\n- c");
    assert_eq!(count(&app, "MARKDOWN_LIST_ITEM"), 3);
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 3);

    let (app, _node) = mount("- [ ]\n\n- [x]\n");
    assert_eq!(count(&app, "MARKDOWN_LIST_ITEM"), 2);
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 2);
}

#[test]
fn unknown_inline_tag_names_lose_the_tag_and_keep_the_text() {
    for (source, expected) in [
        ("a <a/> b", "a  b"),
        ("a <a-b>x</a-b> b", "a x b"),
        ("a <b-tag>c</b-tag> d", "a c d"),
        ("a <code-x>y</code-x> b", "a y b"),
    ] {
        let (app, node) = mount(source);
        let content = texts(&app, node.root());
        assert_eq!(content.len(), 1, "{source}");
        assert_eq!(content[0].text, expected, "{source}");
        assert!(content[0].runs().is_empty(), "{source}");
    }
}

#[test]
fn the_markdown_root_holds_the_appearance_dependency_edge() {
    let (mut app, node) = mount("# Title");
    let window = app.window_id();
    let removed = app
        .app_mut()
        .window_mut(window)
        .expect("window")
        .tree_mut()
        .remove(node.root())
        .expect("remove the markdown root");
    assert!(
        !removed.is_empty(),
        "the root node holds the appearance dependency edge"
    );
}

#[test]
fn the_markdown_tree_leaves_no_numbered_block_ids() {
    let (app, node) = mount("# Title\n\n**bold**\n\n- a\n\n> q");
    assert_eq!(tree_of(&app).debug_id(node.root()), Some("MARKDOWN"));
    let tree = tree_of(&app);
    let mut stack = vec![node.root()];
    while let Some(current) = stack.pop() {
        let entry = tree.get(current).expect("node alive");
        if let Some(id) = tree.debug_id(current) {
            assert!(!id.starts_with("MARKDOWN_BLOCK"), "{id}");
            assert!(!id.starts_with("MARKDOWN_BLANK"), "{id}");
        }
        stack.extend(entry.children());
    }
}

#[test]
fn nested_html_paragraphs_end_at_the_first_closing_tag() {
    let (app, node) = mount("<p>a<p>b</p>c</p>");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "ab");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
    assert_eq!(content[1].text, "c");
    assert_eq!(content[1].style.alignment, TextAlign::Left);
}

#[test]
fn an_html_block_at_the_tag_cap_still_parses_and_one_more_tag_degrades() {
    let at_cap = format!("<p align=\"center\">x</p>{}", "<p>".repeat(4094));
    let (app, node) = mount(&at_cap);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[0].style.alignment, TextAlign::Center);

    let over = format!("<p align=\"center\">x</p>{}", "<p>".repeat(4095));
    let (app, node) = mount(&over);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
}

#[test]
fn an_html_block_beyond_the_payload_cap_degrades_but_keeps_its_text() {
    let under = format!("<p align=\"center\">x</p><!--{}-->", "a".repeat(262_100));
    let (app, node) = mount(&under);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[0].style.alignment, TextAlign::Center);

    let over = format!("<p align=\"center\">x</p><!--{}-->", "a".repeat(262_120));
    let (app, node) = mount(&over);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[0].style.alignment, TextAlign::Left);
}

#[test]
fn the_document_tag_budget_degrades_only_the_blocks_beyond_it() {
    let block = format!("{}x", "<p align=\"center\">".repeat(4000));
    let under = format!("{block}\n\n{block}\n\n{block}\n\n{block}\n");
    let (app, node) = mount(&under);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 4);
    for text in &content {
        assert_eq!(text.text, "x");
        assert_eq!(text.style.alignment, TextAlign::Center);
    }

    let over = format!("{block}\n\n{block}\n\n{block}\n\n{block}\n\n{block}\n");
    let (app, node) = mount(&over);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 5);
    for text in &content[..4] {
        assert_eq!(text.text, "x");
        assert_eq!(text.style.alignment, TextAlign::Center);
    }
    assert_eq!(content[4].text, "x");
    assert_eq!(content[4].style.alignment, TextAlign::Left);
}

#[test]
fn a_cell_beyond_the_measured_character_cap_keeps_its_text_and_measures_a_prefix() {
    let left = "x".repeat(2000);
    let right = "x".repeat(1000);
    let source = format!("| {left} | {right} |\n|---|---|\n");
    let (mut app, node) = mount(&source);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, left);
    assert_eq!(content[1].text, right);
    let widths = cell_widths(&app, node.root());
    assert_eq!(widths.len(), 2);
    assert!(widths[0] + widths[1] <= px(800.5));
    let ratio = widths[0].value() / widths[1].value();
    let capped_left = measured_width(&mut app, &"x".repeat(1024), true) + px(8.0);
    let capped_right = measured_width(&mut app, &right, true) + px(8.0);
    let expected = capped_left.value() / capped_right.value();
    assert!(
        ((ratio - expected) / expected).abs() < 0.05,
        "the rendered width ratio {ratio} follows the capped natural width ratio {expected}"
    );
    assert!(
        ratio < 1.2,
        "the 2000-char cell is measured through its 1024-char prefix, not its full text"
    );
}

#[test]
fn the_document_nested_parse_budget_degrades_later_html_units_without_losing_text() {
    let block = "<p align=\"center\">**b**</p>";
    let source = vec![block; 30].join("\n\n");
    let (app, node) = mount(&source);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 30);
    for text in &content {
        assert_eq!(text.text, "b");
        assert!(!text.text.contains('*'));
    }
    for text in &content[..16] {
        assert_eq!(text.style.alignment, TextAlign::Center);
        assert_eq!(text.runs().len(), 1);
        assert_eq!(text.runs()[0].style.weight, 700);
    }
    for text in &content[16..] {
        assert_eq!(text.style.alignment, TextAlign::Left);
        assert!(text.runs().is_empty());
    }
}

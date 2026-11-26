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

use super::{MarkdownAssets, MarkdownNode, MarkdownStyle, inline::MONO_FAMILY, plain_text};
use crate::theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role};

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
fn plain_text_joins_blocks_and_folds_blank_lines() {
    assert_eq!(plain_text("a\n\nb").as_str(), "a\nb");
    assert_eq!(plain_text("a\n\n\n\nb").as_str(), "a\nb");
    assert_eq!(plain_text("a  \nb").as_str(), "a\nb");
    assert_eq!(plain_text("> quoted").as_str(), "quoted");
    assert_eq!(plain_text("- a\n- b").as_str(), "a\nb");
    assert_eq!(plain_text("```\nx\ny\n```").as_str(), "x\ny");
    assert_eq!(
        plain_text("| a | b |\n|---|---|\n| 1 | 2 |").as_str(),
        "a b\n1 2"
    );
    assert_eq!(plain_text("| a | | c |\n|---|---|---|").as_str(), "a  c");
    assert_eq!(plain_text("a  \nb  \nc").as_str(), "a\nb\nc");
    assert_eq!(plain_text("a\\\n\\\nb").as_str(), "a\n\nb");
    assert_eq!(plain_text("<kbd>Ctrl</kbd><br>K").as_str(), "Ctrl\nK");
    assert_eq!(plain_text("<p>x</p>b<br>c").as_str(), "x\nb\nc");
    assert_eq!(plain_text("<div>x</div>a<br>b").as_str(), "x\na\nb");
    assert_eq!(plain_text("<p>a</p>x<p>b</p>y").as_str(), "a\nb\nx\ny");
    assert_eq!(plain_text("![alt](file://x.png)").as_str(), "");
    assert_eq!(plain_text("<p align=\"center\">x</p>").as_str(), "x");
    assert_eq!(plain_text("").as_str(), "");
}

#[test]
fn the_legacy_plain_text_baseline_is_frozen() {
    assert_eq!(LEGACY_PLAIN_TEXT.len(), 81);
    let mut checked = 0;
    for (key, source, expected) in LEGACY_PLAIN_TEXT {
        let actual = plain_text(source);
        let expected = if BLANK_LINE_COLLAPSED.contains(key) {
            expected.replace("\n\n", "\n")
        } else {
            (*expected).to_owned()
        };
        assert_eq!(actual.as_str(), expected, "{key}");
        checked += 1;
    }
    assert_eq!(checked, 81);
}

const BLANK_LINE_COLLAPSED: &[&str] = &["release:en", "release:zh"];

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
const LEGACY_PLAIN_TEXT: &[(&str, &str, &str)] = &[
    (
        "en:common.market_not_loaded",
        "<p align=\"center\">This widget needs the metadata of a supported market participant to be loaded first.\nLoad the metadata of at least one supported market participant from the <kbd>![](file://icons/phosphoricons/bank.svg)</kbd> button in the top left corner.\nThe market participants this widget supports are:\n%{markets}</p>",
        "This widget needs the metadata of a supported market participant to be loaded first.\nLoad the metadata of at least one supported market participant from the  button in the top left corner.\nThe market participants this widget supports are:\n%{markets}",
    ),
    (
        "en:settings_ui.market_participants.clear_credentials_message",
        "<p align=\"center\">Clear the saved credentials in \"%{title}\"? Trading features that use them will stop working until they are filled in again.</p>",
        "Clear the saved credentials in \"%{title}\"? Trading features that use them will stop working until they are filled in again.",
    ),
    (
        "en:settings_ui.proxies.delete_message",
        "<p align=\"center\">Delete the proxy \"%{name}\"? Network targets set to this proxy go back to the default proxy.</p>",
        "Delete the proxy \"%{name}\"? Network targets set to this proxy go back to the default proxy.",
    ),
    (
        "en:settings_ui.save_failed.message",
        "<p align=\"center\">The settings were not written to disk: %{reason}</p>",
        "The settings were not written to disk: %{reason}",
    ),
    (
        "en:startup.config_reset_message",
        "<p align=\"center\">The configuration file was damaged and has been reset to defaults. The previous file was kept as %{file}.</p>",
        "The configuration file was damaged and has been reset to defaults. The previous file was kept as %{file}.",
    ),
    (
        "en:startup.config_reset_message_no_backup",
        "<p align=\"center\">The configuration file was damaged and has been reset to defaults. The previous file could not be backed up.</p>",
        "The configuration file was damaged and has been reset to defaults. The previous file could not be backed up.",
    ),
    (
        "en:widget.candlestick.select_instrument_prompt",
        "<p align=\"center\">Select an instrument from the picker in the top left</p>",
        "Select an instrument from the picker in the top left",
    ),
    (
        "en:widget.coin_market_cap_fear_and_greed_index.logged_out",
        "<p align=\"center\">You need to register or log in to use this feature.</p>",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.cryptocurrency_etf.logged_out",
        "<p align=\"center\">You need to register or log in to use this feature.</p>",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.empty.hint",
        "Click the <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> button in the upper-right corner to select a widget.",
        "Click the  button in the upper-right corner to select a widget.",
    ),
    (
        "en:widget.ethereum_wallet.empty_addresses",
        "<p align=\"center\">No monitored wallet addresses</p>",
        "No monitored wallet addresses",
    ),
    (
        "en:widget.ethereum_wallet.logged_out",
        "<p align=\"center\">You need to register or log in to use this feature.</p>",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.financial_calendar.logged_out",
        "<p align=\"center\">You need to register or log in to use this feature.</p>",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.financial_index.logged_out",
        "<p align=\"center\">You need to register or log in to use this feature.</p>",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.financial_index.select_at_least_one",
        "<p align=\"center\">Select at least one index</p>",
        "Select at least one index",
    ),
    (
        "en:widget.global_shipping_chokepoint.sign_in",
        "<p align=\"center\">You need to register or log in to see the vessels in the strait.</p>",
        "You need to register or log in to see the vessels in the strait.",
    ),
    (
        "en:widget.hyperliquid_wallet.empty_addresses",
        "<p align=\"center\">No monitored wallets yet — add one from the settings control in the top-left corner.</p>",
        "No monitored wallets yet — add one from the settings control in the top-left corner.",
    ),
    (
        "en:widget.market_trade.select_instrument_prompt",
        "<p align=\"center\">Select an instrument from the picker in the top left</p>",
        "Select an instrument from the picker in the top left",
    ),
    (
        "en:widget.news.logged_out",
        "<p align=\"center\">You need to register or log in to use this feature.</p>",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.order_book.select_instrument_prompt",
        "<p align=\"center\">Select an instrument from the picker in the top left</p>",
        "Select an instrument from the picker in the top left",
    ),
    (
        "en:widget.pentagon_pizza_index.logged_out",
        "<p align=\"center\">You need to register or log in to use this feature.</p>",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.perpetual_statistics.select_instrument_prompt",
        "<p align=\"center\">Select a perpetual instrument from the picker in the top left</p>",
        "Select a perpetual instrument from the picker in the top left",
    ),
    (
        "en:widget.prediction_market.select_event_prompt",
        "<p align=\"center\">Select a prediction event</p>",
        "Select a prediction event",
    ),
    (
        "en:widget.solana_wallet.empty_addresses",
        "<p align=\"center\">No monitored wallet addresses</p>",
        "No monitored wallet addresses",
    ),
    (
        "en:widget.solana_wallet.logged_out",
        "<p align=\"center\">You need to register or log in to use this feature.</p>",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.watch_list.market_metadata_not_loaded",
        "<p align=\"center\">Load the metadata of **%{market}** ![](file://%{logo}?size=16) first</p>",
        "Load the metadata of %{market}  first",
    ),
    (
        "en:widget.x.add_success",
        "<p align=\"center\">Account added successfully</p>",
        "Account added successfully",
    ),
    (
        "en:widget.x.logged_out",
        "<p align=\"center\">You need to register or log in to use this feature.</p>",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:window.account.authorization_complete_message",
        "<p align=\"center\">Authorized wallet %{kind} %{wallet}</p>",
        "Authorized wallet %{kind} %{wallet}",
    ),
    (
        "en:window.account.awaiting_authorization",
        "<p align=\"center\">Finish signing in and authorizing in your browser. Choosing “Cancel sign-in” gives up this attempt.</p>",
        "Finish signing in and authorizing in your browser. Choosing “Cancel sign-in” gives up this attempt.",
    ),
    (
        "en:window.account.sign_out_confirm",
        "<p align=\"center\">Signing out means authorizing again before account features can be used.</p>",
        "Signing out means authorizing again before account features can be used.",
    ),
    (
        "en:window.workspace.add_message",
        "<p align=\"center\">Name the new workspace.</p>",
        "Name the new workspace.",
    ),
    (
        "en:window.workspace.delete_message",
        "<p align=\"center\">Delete the workspace \"%{name}\"? Its pane layout is deleted with it.</p>",
        "Delete the workspace \"%{name}\"? Its pane layout is deleted with it.",
    ),
    (
        "en:window.workspace.name_exists",
        "<p align=\"center\">Another workspace already uses this name</p>",
        "Another workspace already uses this name",
    ),
    (
        "en:window.workspace.name_required",
        "<p align=\"center\">Enter a workspace name</p>",
        "Enter a workspace name",
    ),
    (
        "en:window.workspace.rename_message",
        "<p align=\"center\">Enter the new name of \"%{name}\".</p>",
        "Enter the new name of \"%{name}\".",
    ),
    (
        "zh:common.market_not_loaded",
        "<p align=\"center\">当前部件需要先加载受支持的市场参与者元数据才可以使用。\n请先从左上角的 <kbd>![](file://icons/phosphoricons/bank.svg)</kbd> 按钮加载至少一个受支持的市场参与者元数据。\n本部件支持的市场参与者有：\n%{markets}</p>",
        "当前部件需要先加载受支持的市场参与者元数据才可以使用。\n请先从左上角的  按钮加载至少一个受支持的市场参与者元数据。\n本部件支持的市场参与者有：\n%{markets}",
    ),
    (
        "zh:error.financial_index.duplicate_category",
        "指数清单包含重复分类 `%{code}`",
        "指数清单包含重复分类 %{code}",
    ),
    (
        "zh:error.financial_index.duplicate_index",
        "指数清单包含重复指数 `%{code}`",
        "指数清单包含重复指数 %{code}",
    ),
    (
        "zh:error.financial_index.unknown_category",
        "指数 `%{index}` 引用了未定义的分类 `%{category}`",
        "指数 %{index} 引用了未定义的分类 %{category}",
    ),
    (
        "zh:settings_ui.market_participants.clear_credentials_message",
        "<p align=\"center\">确定清除“%{title}”中已保存的凭据吗？在重新填写前，相关交易功能将无法使用。</p>",
        "确定清除“%{title}”中已保存的凭据吗？在重新填写前，相关交易功能将无法使用。",
    ),
    (
        "zh:settings_ui.proxies.delete_message",
        "<p align=\"center\">确定删除代理「%{name}」吗？指定为该代理的网络目标会回到默认代理。</p>",
        "确定删除代理「%{name}」吗？指定为该代理的网络目标会回到默认代理。",
    ),
    (
        "zh:settings_ui.save_failed.message",
        "<p align=\"center\">设置未能写入磁盘：%{reason}</p>",
        "设置未能写入磁盘：%{reason}",
    ),
    (
        "zh:startup.config_reset_message",
        "<p align=\"center\">配置文件受损，已重新初始化为默认配置。原文件已备份为 %{file}。</p>",
        "配置文件受损，已重新初始化为默认配置。原文件已备份为 %{file}。",
    ),
    (
        "zh:startup.config_reset_message_no_backup",
        "<p align=\"center\">配置文件受损，已重新初始化为默认配置；原文件备份失败。</p>",
        "配置文件受损，已重新初始化为默认配置；原文件备份失败。",
    ),
    (
        "zh:widget.candlestick.select_instrument_prompt",
        "<p align=\"center\">请从左上角的标的物选择器中选择标的</p>",
        "请从左上角的标的物选择器中选择标的",
    ),
    (
        "zh:widget.coin_market_cap_fear_and_greed_index.logged_out",
        "<p align=\"center\">需要注册/登陆后才可以使用</p>",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.cryptocurrency_etf.logged_out",
        "<p align=\"center\">需要注册/登陆后才可以使用</p>",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.empty.hint",
        "请点击右上角的 <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> 按钮选择一个部件",
        "请点击右上角的  按钮选择一个部件",
    ),
    (
        "zh:widget.ethereum_wallet.empty_addresses",
        "<p align=\"center\">暂无监控钱包地址</p>",
        "暂无监控钱包地址",
    ),
    (
        "zh:widget.ethereum_wallet.logged_out",
        "<p align=\"center\">需要注册/登陆后才可以使用</p>",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.financial_calendar.logged_out",
        "<p align=\"center\">需要注册或登录后才能使用该功能。</p>",
        "需要注册或登录后才能使用该功能。",
    ),
    (
        "zh:widget.financial_index.logged_out",
        "<p align=\"center\">需要注册/登陆后才可以使用</p>",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.financial_index.select_at_least_one",
        "<p align=\"center\">请选择至少一个指数</p>",
        "请选择至少一个指数",
    ),
    (
        "zh:widget.global_shipping_chokepoint.sign_in",
        "<p align=\"center\">注册或登录后即可查看海峡内的船舶。</p>",
        "注册或登录后即可查看海峡内的船舶。",
    ),
    (
        "zh:widget.hyperliquid_wallet.empty_addresses",
        "<p align=\"center\">暂无监控钱包，请从左上角设置中添加</p>",
        "暂无监控钱包，请从左上角设置中添加",
    ),
    (
        "zh:widget.market_trade.select_instrument_prompt",
        "<p align=\"center\">请从左上角的标的物选择器中选择标的</p>",
        "请从左上角的标的物选择器中选择标的",
    ),
    (
        "zh:widget.news.logged_out",
        "<p align=\"center\">需要注册/登陆后才可以使用</p>",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.order_book.select_instrument_prompt",
        "<p align=\"center\">请从左上角的标的物选择器中选择标的</p>",
        "请从左上角的标的物选择器中选择标的",
    ),
    (
        "zh:widget.pentagon_pizza_index.logged_out",
        "<p align=\"center\">需要注册/登陆后才可以使用</p>",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.perpetual_statistics.select_instrument_prompt",
        "<p align=\"center\">请从左上角的标的物选择器中选择永续合约标的</p>",
        "请从左上角的标的物选择器中选择永续合约标的",
    ),
    (
        "zh:widget.prediction_market.select_event_prompt",
        "<p align=\"center\">请选择一个预测事件</p>",
        "请选择一个预测事件",
    ),
    (
        "zh:widget.solana_wallet.empty_addresses",
        "<p align=\"center\">暂无监控钱包地址</p>",
        "暂无监控钱包地址",
    ),
    (
        "zh:widget.solana_wallet.logged_out",
        "<p align=\"center\">需要注册/登陆后才可以使用</p>",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.watch_list.market_metadata_not_loaded",
        "<p align=\"center\">请先加载 ![](file://%{logo}?size=16) **%{market}** 的元数据</p>",
        "请先加载  %{market} 的元数据",
    ),
    (
        "zh:widget.x.add_success",
        "<p align=\"center\">账号添加成功</p>",
        "账号添加成功",
    ),
    (
        "zh:widget.x.logged_out",
        "<p align=\"center\">需要注册/登陆后才可以使用</p>",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:window.account.authorization_complete_message",
        "<p align=\"center\">已授权钱包 %{kind} %{wallet}</p>",
        "已授权钱包 %{kind} %{wallet}",
    ),
    (
        "zh:window.account.awaiting_authorization",
        "<p align=\"center\">请在浏览器中完成登录/授权流程。点击“取消登录”可放弃本次登录。</p>",
        "请在浏览器中完成登录/授权流程。点击“取消登录”可放弃本次登录。",
    ),
    (
        "zh:window.account.sign_out_confirm",
        "<p align=\"center\">退出登录后需要重新授权才能使用账户功能。</p>",
        "退出登录后需要重新授权才能使用账户功能。",
    ),
    (
        "zh:window.workspace.add_message",
        "<p align=\"center\">为新工作区命名。</p>",
        "为新工作区命名。",
    ),
    (
        "zh:window.workspace.delete_message",
        "<p align=\"center\">确定删除工作区「%{name}」吗？其中的面板布局也会一并删除。</p>",
        "确定删除工作区「%{name}」吗？其中的面板布局也会一并删除。",
    ),
    (
        "zh:window.workspace.name_exists",
        "<p align=\"center\">该名称已被其它工作区使用</p>",
        "该名称已被其它工作区使用",
    ),
    (
        "zh:window.workspace.name_required",
        "<p align=\"center\">请输入工作区名称</p>",
        "请输入工作区名称",
    ),
    (
        "zh:window.workspace.rename_message",
        "<p align=\"center\">输入「%{name}」的新名称。</p>",
        "输入「%{name}」的新名称。",
    ),
    (
        "en:prod:widget.watch_list.market_metadata_not_loaded",
        "<p align=\"center\">Load the metadata of **Binance** ![](file://brands/Binance/binance.png?size=16) first</p>",
        "Load the metadata of Binance  first",
    ),
    (
        "en:prod:settings_ui.proxies.delete_message",
        "<p align=\"center\">Delete the proxy \"Example Proxy\"? Network targets set to this proxy go back to the default proxy.</p>",
        "Delete the proxy \"Example Proxy\"? Network targets set to this proxy go back to the default proxy.",
    ),
    (
        "zh:prod:widget.watch_list.market_metadata_not_loaded",
        "<p align=\"center\">请先加载 ![](file://brands/Binance/binance.png?size=16) **币安** 的元数据</p>",
        "请先加载  币安 的元数据",
    ),
    (
        "zh:prod:settings_ui.proxies.delete_message",
        "<p align=\"center\">确定删除代理「Example Proxy」吗？指定为该代理的网络目标会回到默认代理。</p>",
        "确定删除代理「Example Proxy」吗？指定为该代理的网络目标会回到默认代理。",
    ),
    (
        "release:en",
        "# Kytrix v0.0.3\r\n\r\n## Added\r\n\r\n**Candlestick indicators**\r\n\r\nCandlestick charts now support technical indicators. A new **Chart settings** button in the header, to the left of the adjustment and interval selectors, opens a settings panel on the right side of the chart and narrows the chart area.\r\n\r\nThe main chart supports MA and EMA, and the sub charts support VOL, MACD, RSI and KDJ. Several sub charts can be enabled at once and stack from top to bottom in the order VOL → MACD → RSI → KDJ. For MA, EMA and RSI, each line's period (1–500) and colour can be set, and lines can be added or removed individually; MACD has short, long and signal periods, with separate toggles and colours for DIF and DEA, and also draws a histogram; KDJ has a calculation period and two smoothing periods, with separate toggles and colours for K, D and J; VOL has no settings.\r\n\r\nIndicator colours are set with a new colour picker: choose a swatch from the palette, adjust the HSLA sliders, or type a hexadecimal colour. Each indicator's enabled state, settings and colours are saved with the workspace; VOL and MA (5, 10, 20) are enabled by default. The top-left corner of the main chart and of each sub chart shows the name, parameters and current value of each line or indicator, and moving the crosshair reveals the value at the corresponding bar. When you enable an indicator that needs more history, the chart fetches the required history automatically.\r\n\r\n**Global Shipping Chokepoint**\r\n\r\nThe new **Global Shipping Chokepoint** widget tracks vessels in the Strait of Hormuz using live AIS data. The basemap is drawn from OpenStreetMap coastlines, borders and roads, with separate colour schemes for the light and dark appearances, and names for 5 countries, 31 cities and 2 bodies of water (some city names appear as you zoom in).\r\n\r\nThe map supports drag-to-pan and wheel or pinch zoom (1×–4×). Vessels are coloured by 20 ship types, with the bow direction following their course; hovering over a vessel shows 16 fields, including its name, MMSI, IMO, call sign, flag, ship type, navigation status, speed, course, heading, destination, ETA, draught, dimensions, and the last update times for dynamic and static data. The header can filter by ship type; the legend keeps every type visible (types excluded by the filter are greyed out), and the vessel count reflects the current filter. If the data has not updated for more than 5 minutes, the widget reports the current picture as failed and offers a recovery action instead of continuing to present it as live data.\r\n\r\nThe widget requires sign-in, and the account must include an AIS vessel data subscription; without one, you can view subscription plans from the widget.\r\n\r\n**Financial Calendar**\r\n\r\nThe new **Financial Calendar** widget lists upcoming global financial events by date. The table has four columns — Importance, Time, Country and Name — and events are grouped by day; importance is shown as dots (one to three for low, medium and high respectively), and the event's reference period (month, quarter and so on) appears beside its name.\r\n\r\nThe header can filter by importance, date and country, and reset all filters with one click. All times are shown in the **Data time zone** setting, and changing the time zone re-groups events by day immediately; all-day events are never shifted to an adjacent date. The calendar refreshes automatically every 10 minutes, with a countdown to the next refresh and a manual refresh button in the header. The widget requires sign-in.\r\n\r\n**Pentagon Pizza Index**\r\n\r\nThe new **Pentagon Pizza Index** widget shows the latest observation of the index. The index is a five-level readiness scale, from Level 1 (\"Maximum readiness\") to Level 5 (\"Lowest state of readiness\"); the card is coloured by level and shows the level number and observation time. The refresh button in the header shows a countdown to the next automatic reading (every 10 minutes) and can also refresh immediately. The widget requires sign-in.\r\n\r\n**CoinMarketCap Fear & Greed Index**\r\n\r\nThe new **CoinMarketCap Fear & Greed Index** widget uses a semicircular gauge to show the current index value and its level (Extreme Fear, Fear, Neutral, Greed or Extreme Greed), with historical values for yesterday, last week, last month and last year listed below, as well as the yearly high and low over the past 365 calendar days, the dates on which they occurred and the levels at the time. The widget refreshes automatically every 60 seconds and can also be refreshed manually. The widget requires sign-in.\r\n\r\n## Changed\r\n\r\n**Wallet monitoring split**\r\n\r\nThe former Wallet Monitor has been split into two independent widgets, Ethereum Wallet Monitor and Solana Wallet Monitor, each using the address format and monitoring API for its chain. The wallet address list no longer shows Active/Paused status or update times, and is sorted by when each address was added; the management dialog is now chain-specific.\r\n\r\n**Settings navigation**\r\n\r\nThe settings window now has a navigation sidebar on the left and page content on the right: group headings no longer collapse, and pages are listed directly beneath them. Search rebuilds the sidebar as you type, keeping only matching pages and groups, and automatically switches to the first matching page when the current page no longer matches.\r\n\r\n**Update dialog**\r\n\r\nThe app update entry point is now an icon-only button. The update dialog has been redesigned: release notes are shown in a wider panel with a persistent scrollbar, download status and progress are centred, and the old dialog title and hover tooltip have been removed.\r\n\r\n**Widget names and categories**\r\n\r\nTweet Monitor has been renamed to X; ETF Flow has been renamed to Crypto Currency ETF and moved from the Market category to Statistics; Hyperliquid Wallet Monitor has been renamed to Hyperliquid Wallet.\r\n\r\n**Memory use**\r\n\r\nLoading the full market catalogues for several venues now retains about 57 MiB of heap instead of about 125 MiB, and the load peak falls from about 143 MiB to about 90 MiB. Refreshing an already-loaded venue no longer keeps a full copy of the old catalogue, so repeated refreshes no longer accumulate memory.\r\n\r\n## Fixed\r\n\r\n**Candlestick history**\r\n\r\nWhen a chart follows the live edge, widening the window or zooming out until more than one page of candles is visible no longer leaves a blank strip on the left that cannot be filled. History is now prefetched continuously as the viewport needs it, and prefetching no longer evicts the latest candle you are viewing.\r\n\r\n**macOS title bar**\r\n\r\nOn macOS, the entire height of the custom title bar can now be used to drag the window; previously only the upper part worked. Double-clicking the title bar maximises or restores the window, and clicking tabs or buttons no longer starts an accidental drag.\r\n\r\n**Singular forms**\r\n\r\nIn the English interface, count-dependent text now uses the singular form when the value is 1, so wording such as \"1 wallets\" or \"1 indices\" no longer appears.\r\n",
        "Kytrix v0.0.3\n\nAdded\n\nCandlestick indicators\n\nCandlestick charts now support technical indicators. A new Chart settings button in the header, to the left of the adjustment and interval selectors, opens a settings panel on the right side of the chart and narrows the chart area.\n\nThe main chart supports MA and EMA, and the sub charts support VOL, MACD, RSI and KDJ. Several sub charts can be enabled at once and stack from top to bottom in the order VOL → MACD → RSI → KDJ. For MA, EMA and RSI, each line's period (1–500) and colour can be set, and lines can be added or removed individually; MACD has short, long and signal periods, with separate toggles and colours for DIF and DEA, and also draws a histogram; KDJ has a calculation period and two smoothing periods, with separate toggles and colours for K, D and J; VOL has no settings.\n\nIndicator colours are set with a new colour picker: choose a swatch from the palette, adjust the HSLA sliders, or type a hexadecimal colour. Each indicator's enabled state, settings and colours are saved with the workspace; VOL and MA (5, 10, 20) are enabled by default. The top-left corner of the main chart and of each sub chart shows the name, parameters and current value of each line or indicator, and moving the crosshair reveals the value at the corresponding bar. When you enable an indicator that needs more history, the chart fetches the required history automatically.\n\nGlobal Shipping Chokepoint\n\nThe new Global Shipping Chokepoint widget tracks vessels in the Strait of Hormuz using live AIS data. The basemap is drawn from OpenStreetMap coastlines, borders and roads, with separate colour schemes for the light and dark appearances, and names for 5 countries, 31 cities and 2 bodies of water (some city names appear as you zoom in).\n\nThe map supports drag-to-pan and wheel or pinch zoom (1×–4×). Vessels are coloured by 20 ship types, with the bow direction following their course; hovering over a vessel shows 16 fields, including its name, MMSI, IMO, call sign, flag, ship type, navigation status, speed, course, heading, destination, ETA, draught, dimensions, and the last update times for dynamic and static data. The header can filter by ship type; the legend keeps every type visible (types excluded by the filter are greyed out), and the vessel count reflects the current filter. If the data has not updated for more than 5 minutes, the widget reports the current picture as failed and offers a recovery action instead of continuing to present it as live data.\n\nThe widget requires sign-in, and the account must include an AIS vessel data subscription; without one, you can view subscription plans from the widget.\n\nFinancial Calendar\n\nThe new Financial Calendar widget lists upcoming global financial events by date. The table has four columns — Importance, Time, Country and Name — and events are grouped by day; importance is shown as dots (one to three for low, medium and high respectively), and the event's reference period (month, quarter and so on) appears beside its name.\n\nThe header can filter by importance, date and country, and reset all filters with one click. All times are shown in the Data time zone setting, and changing the time zone re-groups events by day immediately; all-day events are never shifted to an adjacent date. The calendar refreshes automatically every 10 minutes, with a countdown to the next refresh and a manual refresh button in the header. The widget requires sign-in.\n\nPentagon Pizza Index\n\nThe new Pentagon Pizza Index widget shows the latest observation of the index. The index is a five-level readiness scale, from Level 1 (\"Maximum readiness\") to Level 5 (\"Lowest state of readiness\"); the card is coloured by level and shows the level number and observation time. The refresh button in the header shows a countdown to the next automatic reading (every 10 minutes) and can also refresh immediately. The widget requires sign-in.\n\nCoinMarketCap Fear & Greed Index\n\nThe new CoinMarketCap Fear & Greed Index widget uses a semicircular gauge to show the current index value and its level (Extreme Fear, Fear, Neutral, Greed or Extreme Greed), with historical values for yesterday, last week, last month and last year listed below, as well as the yearly high and low over the past 365 calendar days, the dates on which they occurred and the levels at the time. The widget refreshes automatically every 60 seconds and can also be refreshed manually. The widget requires sign-in.\n\nChanged\n\nWallet monitoring split\n\nThe former Wallet Monitor has been split into two independent widgets, Ethereum Wallet Monitor and Solana Wallet Monitor, each using the address format and monitoring API for its chain. The wallet address list no longer shows Active/Paused status or update times, and is sorted by when each address was added; the management dialog is now chain-specific.\n\nSettings navigation\n\nThe settings window now has a navigation sidebar on the left and page content on the right: group headings no longer collapse, and pages are listed directly beneath them. Search rebuilds the sidebar as you type, keeping only matching pages and groups, and automatically switches to the first matching page when the current page no longer matches.\n\nUpdate dialog\n\nThe app update entry point is now an icon-only button. The update dialog has been redesigned: release notes are shown in a wider panel with a persistent scrollbar, download status and progress are centred, and the old dialog title and hover tooltip have been removed.\n\nWidget names and categories\n\nTweet Monitor has been renamed to X; ETF Flow has been renamed to Crypto Currency ETF and moved from the Market category to Statistics; Hyperliquid Wallet Monitor has been renamed to Hyperliquid Wallet.\n\nMemory use\n\nLoading the full market catalogues for several venues now retains about 57 MiB of heap instead of about 125 MiB, and the load peak falls from about 143 MiB to about 90 MiB. Refreshing an already-loaded venue no longer keeps a full copy of the old catalogue, so repeated refreshes no longer accumulate memory.\n\nFixed\n\nCandlestick history\n\nWhen a chart follows the live edge, widening the window or zooming out until more than one page of candles is visible no longer leaves a blank strip on the left that cannot be filled. History is now prefetched continuously as the viewport needs it, and prefetching no longer evicts the latest candle you are viewing.\n\nmacOS title bar\n\nOn macOS, the entire height of the custom title bar can now be used to drag the window; previously only the upper part worked. Double-clicking the title bar maximises or restores the window, and clicking tabs or buttons no longer starts an accidental drag.\n\nSingular forms\n\nIn the English interface, count-dependent text now uses the singular form when the value is 1, so wording such as \"1 wallets\" or \"1 indices\" no longer appears.",
    ),
    (
        "release:zh",
        "# Kytrix v0.0.3\r\n\r\n## 新增\r\n\r\n**K 线指标**\r\n\r\nK 线图现在支持技术指标。标题栏右侧新增「图表设置」按钮（位于复权与周期选择左侧），打开后在图表右侧展开设置面板，图表区域随之收窄。\r\n\r\n主图支持 MA 与 EMA，副图支持 VOL、MACD、RSI 与 KDJ。可同时启用多个副图，按 VOL → MACD → RSI → KDJ 的顺序自上而下排列。MA、EMA 与\r\nRSI 的指标线可逐条设置周期（1–500）和颜色，并可新增或删除；MACD 可设置短周期、长周期与移动平均周期，分别控制 DIF、DEA\r\n的显示和颜色，并绘制柱状图；KDJ 可设置计算周期与两个移动平均周期，分别控制 K、D、J 的显示和颜色；VOL 无需设置。\r\n\r\n指标颜色由新的颜色选择器设置：可从色板挑选，也可用 HSLA 滑杆调整，或直接输入十六进制颜色。所有指标的启用状态、参数与颜色都会随工作区保存；默认启用\r\nVOL 与 MA (5, 10, 20)。主图与各副图左上角会列出指标线或指标的名称、参数与当前值，十字光标移动时同步显示对应柱位的数值。启用需要更多历史数据的指标时，图表会自动补取所需历史。\r\n\r\n**全球关键航道**\r\n\r\n新增「全球关键航道」组件，以实时 AIS 数据监控霍尔木兹海峡的船舶。底图使用 OpenStreetMap 的海岸线、国界与道路绘制，亮色与暗色外观各有一套配色，并标注\r\n5 个国家、31 个城市与 2 个水域的名称（部分城市名随缩放显示）。\r\n\r\n地图支持拖动平移，以及滚轮与双指缩放（1×–4×）。船舶按 20\r\n种船型着色，船头方向跟随航向；将指针移到船舶上可查看船名、MMSI、IMO、呼号、船旗国、船型、航行状态、航速、航向、艏向、目的港、预计到达、吃水、尺寸，以及动态与静态数据更新时间等\r\n16 项信息。标题栏可按船型筛选，图例保留全部船型（被筛掉的类型置灰），船舶数量显示当前筛选结果。数据超过 5\r\n分钟未更新时，组件会把当前画面报告为失败并提供恢复入口，而不是继续把它当作实时数据。\r\n\r\n该组件需要登录，且账户需包含 AIS 船舶数据订阅；没有订阅时可从组件内查看订阅方案。\r\n\r\n**财经日历**\r\n\r\n新增「财经日历」组件，按日期列出未来的全球财经事件。表格分为重要性、时间、国家与名称四列，事件按天分组；重要性以圆点数量表示（低、中、高分别为一至三个），名称旁会附带事件对应的月份、季度等参考期。\r\n\r\n标题栏可按重要性、日期与国家筛选，并可一键重置。所有时间按「数据时间时区」显示，切换时区后日期分组会同步重算；全天事件不会被换算到相邻日期。日历每\r\n10 分钟自动刷新，标题栏同时显示距离下次刷新的倒计时和手动刷新按钮。该组件需要登录后使用。\r\n\r\n**五角大楼披萨指数**\r\n\r\n新增「五角大楼披萨指数」组件，展示该指数的最新观测。指数以五级战备等级表示，从等级 1「最高戒备」到等级\r\n5「最低戒备」；卡片按等级着色，并显示等级编号与观测时间。标题栏的刷新按钮会显示距离下一次自动读取的倒计时（每 10\r\n分钟自动读取一次），也可以立即刷新。该组件需要登录后使用。\r\n\r\n**CoinMarketCap 恐惧贪婪指数**\r\n\r\n新增「CoinMarketCap 恐惧贪婪指数」组件：半圆仪表显示当前指数值与所处档位（极度恐惧、恐惧、中性、贪婪、极度贪婪），下方列出昨天、上周、上月与去年的历史值，以及过去\r\n365 个日历日的年度最高/最低值、发生日期与当时的档位。组件每 60 秒自动刷新，也可手动刷新。该组件需要登录后使用。\r\n\r\n## 变更\r\n\r\n**钱包监控拆分**\r\n\r\n原「钱包监控」拆分为「Ethereum 钱包监控」与「Solana\r\n钱包监控」两个独立组件，各自使用对应链的地址格式和监控接口。钱包地址列表不再显示「已激活 / 已暂停」状态与更新时间，改为按添加时间排序；管理对话框也按链区分。\r\n\r\n**设置界面**\r\n\r\n设置窗口改为左侧导航栏加右侧内容页：分组标题不再折叠，页面直接列在标题下；搜索会实时重建设置栏，只保留命中的页面与分组，并在当前页不再匹配时自动切到第一个匹配页面。\r\n\r\n**更新对话框**\r\n\r\n应用更新入口改为图标按钮；更新对话框重新布局，发布说明使用更宽的面板并带常驻滚动条，下载状态与进度居中显示，旧有的对话框标题与悬浮提示已移除。\r\n\r\n**组件名称与分类**\r\n\r\n「推文监控」更名为「X」；「ETF 资金流」更名为「加密货币ETF」，并从「行情」分类移入「统计」分类；「Hyperliquid 钱包监控」更名为「Hyperliquid\r\n钱包」。\r\n\r\n**内存占用**\r\n\r\n加载多个场所的完整行情目录时，常驻内存由约 125 MiB 降至约 57 MiB，加载峰值由约 143 MiB 降至约 90\r\nMiB；刷新已加载的场所不再保留整份旧目录，反复刷新也不会持续累积内存。\r\n\r\n## 修复\r\n\r\n**K 线历史数据**\r\n\r\n图表跟随最新行情时，窗口加宽，或将 K 线缩放到一屏显示超过一页后，左侧不再留下无法填充的空白；现在会按视口需要连续预取历史，并且预取不会挤掉正在查看的最新一根\r\nK 线。\r\n\r\n**macOS 标题栏**\r\n\r\nmacOS 上自定义标题栏的整个高度现在都可以拖动窗口，此前只有上半部分有效；双击标题栏可最大化或还原，点击标签页与按钮也不会误触发拖动。\r\n\r\n**数量文案**\r\n\r\n英文界面中，数值为 1 的计数文案改用单数形式，不再出现「1 wallets」「1 indices」等写法。\r\n",
        "Kytrix v0.0.3\n\n新增\n\nK 线指标\n\nK 线图现在支持技术指标。标题栏右侧新增「图表设置」按钮（位于复权与周期选择左侧），打开后在图表右侧展开设置面板，图表区域随之收窄。\n\n主图支持 MA 与 EMA，副图支持 VOL、MACD、RSI 与 KDJ。可同时启用多个副图，按 VOL → MACD → RSI → KDJ 的顺序自上而下排列。MA、EMA 与\nRSI 的指标线可逐条设置周期（1–500）和颜色，并可新增或删除；MACD 可设置短周期、长周期与移动平均周期，分别控制 DIF、DEA\n的显示和颜色，并绘制柱状图；KDJ 可设置计算周期与两个移动平均周期，分别控制 K、D、J 的显示和颜色；VOL 无需设置。\n\n指标颜色由新的颜色选择器设置：可从色板挑选，也可用 HSLA 滑杆调整，或直接输入十六进制颜色。所有指标的启用状态、参数与颜色都会随工作区保存；默认启用\nVOL 与 MA (5, 10, 20)。主图与各副图左上角会列出指标线或指标的名称、参数与当前值，十字光标移动时同步显示对应柱位的数值。启用需要更多历史数据的指标时，图表会自动补取所需历史。\n\n全球关键航道\n\n新增「全球关键航道」组件，以实时 AIS 数据监控霍尔木兹海峡的船舶。底图使用 OpenStreetMap 的海岸线、国界与道路绘制，亮色与暗色外观各有一套配色，并标注\n5 个国家、31 个城市与 2 个水域的名称（部分城市名随缩放显示）。\n\n地图支持拖动平移，以及滚轮与双指缩放（1×–4×）。船舶按 20\n种船型着色，船头方向跟随航向；将指针移到船舶上可查看船名、MMSI、IMO、呼号、船旗国、船型、航行状态、航速、航向、艏向、目的港、预计到达、吃水、尺寸，以及动态与静态数据更新时间等\n16 项信息。标题栏可按船型筛选，图例保留全部船型（被筛掉的类型置灰），船舶数量显示当前筛选结果。数据超过 5\n分钟未更新时，组件会把当前画面报告为失败并提供恢复入口，而不是继续把它当作实时数据。\n\n该组件需要登录，且账户需包含 AIS 船舶数据订阅；没有订阅时可从组件内查看订阅方案。\n\n财经日历\n\n新增「财经日历」组件，按日期列出未来的全球财经事件。表格分为重要性、时间、国家与名称四列，事件按天分组；重要性以圆点数量表示（低、中、高分别为一至三个），名称旁会附带事件对应的月份、季度等参考期。\n\n标题栏可按重要性、日期与国家筛选，并可一键重置。所有时间按「数据时间时区」显示，切换时区后日期分组会同步重算；全天事件不会被换算到相邻日期。日历每\n10 分钟自动刷新，标题栏同时显示距离下次刷新的倒计时和手动刷新按钮。该组件需要登录后使用。\n\n五角大楼披萨指数\n\n新增「五角大楼披萨指数」组件，展示该指数的最新观测。指数以五级战备等级表示，从等级 1「最高戒备」到等级\n5「最低戒备」；卡片按等级着色，并显示等级编号与观测时间。标题栏的刷新按钮会显示距离下一次自动读取的倒计时（每 10\n分钟自动读取一次），也可以立即刷新。该组件需要登录后使用。\n\nCoinMarketCap 恐惧贪婪指数\n\n新增「CoinMarketCap 恐惧贪婪指数」组件：半圆仪表显示当前指数值与所处档位（极度恐惧、恐惧、中性、贪婪、极度贪婪），下方列出昨天、上周、上月与去年的历史值，以及过去\n365 个日历日的年度最高/最低值、发生日期与当时的档位。组件每 60 秒自动刷新，也可手动刷新。该组件需要登录后使用。\n\n变更\n\n钱包监控拆分\n\n原「钱包监控」拆分为「Ethereum 钱包监控」与「Solana\n钱包监控」两个独立组件，各自使用对应链的地址格式和监控接口。钱包地址列表不再显示「已激活 / 已暂停」状态与更新时间，改为按添加时间排序；管理对话框也按链区分。\n\n设置界面\n\n设置窗口改为左侧导航栏加右侧内容页：分组标题不再折叠，页面直接列在标题下；搜索会实时重建设置栏，只保留命中的页面与分组，并在当前页不再匹配时自动切到第一个匹配页面。\n\n更新对话框\n\n应用更新入口改为图标按钮；更新对话框重新布局，发布说明使用更宽的面板并带常驻滚动条，下载状态与进度居中显示，旧有的对话框标题与悬浮提示已移除。\n\n组件名称与分类\n\n「推文监控」更名为「X」；「ETF 资金流」更名为「加密货币ETF」，并从「行情」分类移入「统计」分类；「Hyperliquid 钱包监控」更名为「Hyperliquid\n钱包」。\n\n内存占用\n\n加载多个场所的完整行情目录时，常驻内存由约 125 MiB 降至约 57 MiB，加载峰值由约 143 MiB 降至约 90\nMiB；刷新已加载的场所不再保留整份旧目录，反复刷新也不会持续累积内存。\n\n修复\n\nK 线历史数据\n\n图表跟随最新行情时，窗口加宽，或将 K 线缩放到一屏显示超过一页后，左侧不再留下无法填充的空白；现在会按视口需要连续预取历史，并且预取不会挤掉正在查看的最新一根\nK 线。\n\nmacOS 标题栏\n\nmacOS 上自定义标题栏的整个高度现在都可以拖动窗口，此前只有上半部分有效；双击标题栏可最大化或还原，点击标签页与按钮也不会误触发拖动。\n\n数量文案\n\n英文界面中，数值为 1 的计数文案改用单数形式，不再出现「1 wallets」「1 indices」等写法。",
    ),
];

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
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 1);
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
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 1);
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

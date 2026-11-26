use ui_core::{
    interaction::ScrollConfig,
    math::{LogicalPoint, LogicalSize, px},
    node::builder::NodeBuilder,
    paint::{color::Color, style_builder::StyleBuilder},
    text::TextStyle,
};

use super::{
    EmbeddedAssets, MarkdownNode, bounds, children, count, glyph_run, measured_width, mount,
    node_count, nodes_with_id, resolved, style, texts, tree_of,
};
use crate::{
    kit::markdown::plain_text,
    theme::{appearance::Appearance, handle::ThemeHandle},
};

fn assert_underline(style: &TextStyle, color: Color) {
    let markdown = super::style();
    let line = style.decorations().underline.expect("the run underline");
    assert_eq!(line.thickness, markdown.decoration_thickness);
    assert_eq!(line.color, Some(color));
}

fn assert_background(style: &TextStyle) {
    let markdown = super::style();
    let background = style.background().expect("the run background");
    assert_eq!(background.color, markdown.code_background);
    assert_eq!(background.corner_radius, markdown.background_radius);
    assert_eq!(background.padding_x, markdown.background_padding_x);
}

fn assert_strikethrough(style: &TextStyle) {
    let markdown = super::style();
    let line = style
        .decorations()
        .strikethrough
        .expect("the run strikethrough");
    assert_eq!(line.thickness, markdown.decoration_thickness);
    assert_eq!(line.color, Some(markdown.foreground));
}

#[test]
fn a_deep_emphasis_drop_keeps_the_sibling_text() {
    let source = format!("keep {}x{} tail", "*".repeat(200), "*".repeat(200));
    let (app, node) = mount(&source);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "keep  tail");
    assert!(content[0].runs().is_empty());
    assert_eq!(node_count(&app, node.root()), 2);
}

#[test]
fn the_deep_nesting_guard_applies_inside_containers() {
    let source = format!("> keep {}x{}", "*".repeat(200), "*".repeat(200));
    let (app, node) = mount(&source);
    assert_eq!(count(&app, "MARKDOWN_QUOTE"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "keep ");
    assert!(content[0].runs().is_empty());
}

#[test]
fn deeply_nested_lists_are_capped_at_thirty_two_levels() {
    let source = format!("{}x", "- ".repeat(300));
    let (app, node) = mount(&source);
    assert_eq!(count(&app, "MARKDOWN_LIST"), 32);
    assert_eq!(count(&app, "MARKDOWN_LIST_ITEM"), 32);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 33);
    assert_eq!(content[32].text, "x");
    assert_eq!(node_count(&app, node.root()), 129);
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 0);
}

#[test]
fn degrading_an_over_deep_container_keeps_its_siblings() {
    let source = format!("{}x\n\nsibling", "> ".repeat(200));
    let (app, node) = mount(&source);
    assert_eq!(count(&app, "MARKDOWN_QUOTE"), 64);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].text, "x");
    assert_eq!(content[1].text, "sibling");
    assert_eq!(count(&app, "MARKDOWN_SPACER"), 1);
}

#[test]
fn styles_inside_a_link_are_kept() {
    let (app, node) = mount("[**b** c](u)");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "b c");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].range, 0..1);
    assert_eq!(runs[0].style.weight, 700);
    assert_eq!(runs[0].style.color, style().link);
    assert_underline(&runs[0].style, style().link);
    assert_eq!(runs[1].range, 1..3);
    assert_eq!(runs[1].style.weight, 400);
    assert_eq!(runs[1].style.color, style().link);
    assert_underline(&runs[1].style, style().link);
}

#[test]
fn nested_kbd_tags_share_the_outermost_run() {
    let (app, node) = mount("<kbd>a<kbd>b</kbd>c</kbd>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "abc");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].range, 0..3);
    assert_underline(&runs[0].style, style().code_foreground);
    assert_background(&runs[0].style);
}

#[test]
fn kbd_marks_the_inline_styles_inside_it() {
    let (app, node) = mount("<kbd>**bold** ![](file://icons/x.png)</kbd>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "bold ");
    assert_eq!(content[0].inline_boxes.len(), 1);
    let runs = content[0].runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].range, 0..4);
    assert_eq!(runs[0].style.weight, 700);
    assert_underline(&runs[0].style, style().code_foreground);
    assert_background(&runs[0].style);
    assert_eq!(runs[1].range, 4..5);
    assert_underline(&runs[1].style, style().code_foreground);
    assert_background(&runs[1].style);
}

#[test]
fn unmatched_kbd_tags_follow_the_parsed_tags() {
    let (app, node) = mount("a</kbd> b");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a b");
    assert!(content[0].runs().is_empty());

    let (app, node) = mount("<kbd>a");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].range, 0..1);
    assert_underline(&runs[0].style, style().code_foreground);
    assert_background(&runs[0].style);
}

#[test]
fn a_soft_break_inside_kbd_keeps_one_run() {
    let (app, node) = mount("<kbd>a\nb</kbd>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a\nb");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].range, 0..3);
    assert_underline(&runs[0].style, style().code_foreground);
}

#[test]
fn an_ignored_image_still_splits_the_text_around_it() {
    let (app, node) = mount("a![x](icon:foo)b");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "axb");
    assert!(content[0].inline_boxes.is_empty());
}

#[test]
fn the_same_source_parses_to_the_same_document() {
    let source = "# H\n\n- a\n- b\n\n| x | y |\n|---|---|\n| 1 | 2 |";
    let (first, first_node) = mount(source);
    let (second, second_node) = mount(source);
    assert_eq!(
        texts(&first, first_node.root())
            .iter()
            .map(|content| content.text.to_string())
            .collect::<Vec<_>>(),
        texts(&second, second_node.root())
            .iter()
            .map(|content| content.text.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(node_count(&first, first_node.root()), 24);
    assert_eq!(node_count(&second, second_node.root()), 24);
    assert_eq!(plain_text(source), plain_text(source));
}

#[test]
fn a_fenced_code_block_takes_the_first_info_word() {
    let (app, node) = mount("```rust ignore\nlet x = 1;\n```");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "let x = 1;");
    assert!(!content[0].text.contains("rust"));
    assert_eq!(
        content[0].style.family().map(|family| family.as_str()),
        Some("Consolas")
    );
}

#[test]
fn a_fenced_code_block_strips_exactly_one_trailing_newline() {
    let (app, node) = mount("```\na\n\n```");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a\n");
}

#[test]
fn a_list_item_may_contain_a_code_block() {
    let (app, node) = mount("- a\n\n  ```\n  x\n  ```");
    assert_eq!(count(&app, "MARKDOWN_LIST_CONTENT"), 1);
    assert_eq!(count(&app, "MARKDOWN_CODE"), 1);
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "•");
    assert_eq!(content[1].text, "a");
    assert_eq!(content[2].text, "x");
}

#[test]
fn multi_byte_offsets_stay_on_char_boundaries() {
    let (app, node) = mount("**中**文");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "中文");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].range, 0..3);
    assert!(content[0].text.is_char_boundary(runs[0].range.start));
    assert!(content[0].text.is_char_boundary(runs[0].range.end));
}

#[test]
fn links_carry_underlines_in_the_paint_stream() {
    let (mut app, node) = mount("[x](u)");
    let text_node = children(&app, node.root())[0];
    let advance = measured_width(&mut app, "x", false);
    let quads = app
        .rendered()
        .expect("the first frame renders")
        .quads
        .iter()
        .filter(|quad| quad.node == text_node)
        .collect::<Vec<_>>();
    assert_eq!(quads.len(), 1);
    assert_eq!(quads[0].bounds.size.height, style().decoration_thickness);
    assert_eq!(quads[0].bounds.size.width, advance);
    assert_eq!(quads[0].fill.components, resolved(style().link));
}

#[test]
fn ordered_markers_have_a_layout_box() {
    let (app, node) = mount("3. x");
    let markers = nodes_with_id(&app, node.root(), "MARKDOWN_LIST_MARKER");
    assert_eq!(markers.len(), 1);
    let marker = bounds(&app, markers[0]);
    assert_eq!(marker.size.width, px(20.0));
    assert_eq!(marker.size.height, style().line_height());
    assert_eq!(texts(&app, node.root())[0].text, "3.");
}

#[test]
fn kbd_and_code_paragraphs_keep_their_runs_and_layout() {
    let (app, node) = mount("text <kbd>K</kbd> `c`");
    assert_eq!(node_count(&app, node.root()), 2);
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "text K c");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].range, 5..6);
    assert_underline(&runs[0].style, style().code_foreground);
    assert_background(&runs[0].style);
    assert_eq!(runs[1].range, 7..8);
    assert_background(&runs[1].style);
    assert!(runs[1].style.decorations().underline.is_none());
    let text_node = children(&app, node.root())[0];
    let text_bounds = bounds(&app, text_node);
    assert!(text_bounds.size.width > px(0.0));
    assert!(text_bounds.size.height > px(0.0));
    let (_, run_width) = glyph_run(&app, text_node);
    assert!(run_width > px(0.0));
}

#[test]
fn releasing_the_markdown_unloads_its_root_and_stops_syncing() {
    let (mut app, mut node) = mount("# Title\n\nparagraph");
    let markdown_root = node.root();
    assert!(tree_of(&app).get(markdown_root).is_ok());
    assert!(node.release(app.app_mut()));
    assert!(tree_of(&app).get(markdown_root).is_err());
    assert!(!node.release(app.app_mut()));
    assert!(!node.sync(app.app_mut()));
}

#[test]
fn rebuilding_keeps_the_root_identity_and_the_page_offset() {
    let mut app = super::TestApp::new();
    let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    let page = NodeBuilder::boxed()
        .id("page")
        .style(
            StyleBuilder::new()
                .relative()
                .width(px(800.0))
                .height(px(600.0))
                .overflow_hidden()
                .build(),
        )
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            root,
        )
        .expect("the page container");
    let handle = app
        .app_mut()
        .make_scroll(window, page, ScrollConfig::default())
        .expect("the scroll container");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(800.0), px(1800.0)))
        .expect("the scroll content");
    let mut node = MarkdownNode::mount(
        app.app_mut(),
        window,
        page,
        theme.clone(),
        "# Title\n\nparagraph\n\nsecond\n\nthird",
        EmbeddedAssets,
    )
    .expect("the markdown mounts");
    let _ = app.drain().expect("the first frame");
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(240.0)))
        .expect("scroll the page");
    let _ = app.drain().expect("the scroll frame");
    let offset = app
        .app()
        .scroll_state(&handle)
        .expect("the scroll state")
        .offset
        .y;
    assert!((offset - px(240.0)).abs() < px(0.5));

    let root_before = node.root();
    let first_child_before = children(&app, root_before)[0];
    assert!(theme.set_appearance(app.app_mut(), Appearance::Dark));
    assert!(node.sync(app.app_mut()));
    let _ = app.drain().expect("the rebuilt frame");

    assert_eq!(node.root(), root_before);
    let tree = tree_of(&app);
    let rebuilt = tree.get(root_before).expect("the root survives");
    assert!(!rebuilt.children().is_empty());
    assert_ne!(rebuilt.children()[0], first_child_before);
    assert_eq!(rebuilt.parent(), Some(page));
    let offset_after = app
        .app()
        .scroll_state(&handle)
        .expect("the scroll state")
        .offset
        .y;
    assert!((offset_after - offset).abs() < px(0.5));
}

#[test]
fn emphasis_nested_within_the_limit_is_kept() {
    let source = format!("{}a{}", "*".repeat(16), "*".repeat(16));
    let (app, node) = mount(&source);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "a");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].style.weight, 700);
}

#[test]
fn emphasis_nested_exactly_at_the_limit_is_kept() {
    let source = format!("{}a{}", "*".repeat(128), "*".repeat(128));
    let (app, node) = mount(&source);
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "a");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].style.weight, 700);
}

#[test]
fn emphasis_nested_past_the_limit_is_dropped() {
    let source = format!("{}a{}", "*".repeat(130), "*".repeat(130));
    let (app, node) = mount(&source);
    assert!(texts(&app, node.root()).is_empty());
    assert_eq!(node_count(&app, node.root()), 1);
}

#[test]
fn seven_hashes_are_not_a_heading() {
    let (app, node) = mount("####### no");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].text, "####### no");
    assert!(content[0].runs().is_empty());
}

#[test]
fn link_style_stops_at_the_end_of_the_link() {
    let (app, node) = mount("[x](u) tail");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "x tail");
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].range, 0..1);
    assert_eq!(runs[0].style.color, style().link);
    assert_underline(&runs[0].style, style().link);
}

#[test]
fn table_cells_keep_their_inline_marks() {
    let (app, node) = mount("| h1 | h2 | h3 |\n|---|---|---|\n| **b** | `c` | ~~d~~ |");
    let content = texts(&app, node.root());
    assert_eq!(content.len(), 6);
    let bold = content[3].runs();
    assert_eq!(bold.len(), 1);
    assert_eq!(bold[0].style.weight, 700);
    let code = content[4].runs();
    assert_eq!(code.len(), 1);
    assert_eq!(
        code[0].style.family().map(|family| family.as_str()),
        Some("Consolas")
    );
    let strike = content[5].runs();
    assert_eq!(strike.len(), 1);
    assert_strikethrough(&strike[0].style);
}

#[test]
fn heading_style_bolds_every_run() {
    let (app, node) = mount("# *a*");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a");
    assert_eq!(content[0].style.size, style().body_size * 1.75);
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].style.weight, 700);
    assert_eq!(runs[0].style.slant, ui_core::text::TextSlant::Italic);
}

#[test]
fn inline_code_uses_monospace_and_the_code_color() {
    let (app, node) = mount("`c`");
    let content = texts(&app, node.root());
    let runs = content[0].runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].style.family().map(|family| family.as_str()),
        Some("Consolas")
    );
    assert_eq!(runs[0].style.color, style().code_foreground);
    let background = runs[0].style.background().expect("the code background");
    assert_eq!(background.color, style().code_background);
}

#[test]
fn hard_breaks_become_newline_bytes_and_reset_marks() {
    let (app, node) = mount("a  \nb");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "a\nb");
    assert!(content[0].runs().is_empty());
}

#[test]
fn inline_images_keep_their_text_offset() {
    let (app, node) = mount("ab![](file://icons/x.png)cd");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "abcd");
    assert_eq!(content[0].inline_boxes.len(), 1);
    assert_eq!(content[0].inline_boxes[0].index, 2);

    let (app, node) = mount("![](file://icons/x.png)");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "\u{200B}");
    assert_eq!(content[0].inline_boxes[0].index, 0);
}

#[test]
fn tight_and_loose_nested_lists_take_different_shapes() {
    let (tight_app, tight_node) = mount("- a\n  - b");
    assert_eq!(count(&tight_app, "MARKDOWN_LIST_CONTENT_TIGHT"), 1);
    assert_eq!(count(&tight_app, "MARKDOWN_LIST_CONTENT"), 0);
    let (loose_app, loose_node) = mount("- a\n\n  - b");
    assert_eq!(count(&loose_app, "MARKDOWN_LIST_CONTENT"), 1);
    assert_eq!(count(&loose_app, "MARKDOWN_LIST_CONTENT_TIGHT"), 0);
    assert_eq!(node_count(&tight_app, tight_node.root()), 10);
    assert_eq!(node_count(&loose_app, loose_node.root()), 10);
    let tight_outline = super::outline::outline(&tight_app, tight_node.root());
    let loose_outline = super::outline::outline(&loose_app, loose_node.root());
    assert!(tight_outline.contains(&"  item[tight]".to_owned()));
    assert!(loose_outline.contains(&"  item[loose]".to_owned()));
    assert_ne!(tight_outline, loose_outline);
}

#[test]
fn the_line_break_model_does_not_distinguish_hard_from_soft() {
    let (soft_app, soft_node) = mount("a\nb");
    let (hard_app, hard_node) = mount("a\\\nb");
    assert_eq!(texts(&soft_app, soft_node.root())[0].text, "a\nb");
    assert_eq!(texts(&hard_app, hard_node.root())[0].text, "a\nb");
    assert_eq!(node_count(&soft_app, soft_node.root()), 2);
    assert_eq!(node_count(&hard_app, hard_node.root()), 2);
    assert_eq!(
        super::outline::outline(&soft_app, soft_node.root()),
        super::outline::outline(&hard_app, hard_node.root())
    );
}

#[test]
fn kbd_around_an_image_keeps_only_the_image_box() {
    let (app, node) = mount("<kbd>![](file://icons/x.png)</kbd>");
    let content = texts(&app, node.root());
    assert_eq!(content[0].text, "\u{200B}");
    assert_eq!(content[0].inline_boxes.len(), 1);
    assert!(content[0].runs().is_empty());
}

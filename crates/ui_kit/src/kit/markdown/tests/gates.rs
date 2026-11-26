use taffy::style::LengthPercentage;
use ui_core::{
    math::{LogicalPx, px},
    node::{id::NodeId, tree::NodeKind},
    test_support::app::TestApp,
    window::FrameOutcome,
};

use super::{
    EN_RELEASE_NOTES, ZH_RELEASE_NOTES, bounds, cell_widths, children, count,
    fixtures::i18n_markdown::{I18N_MARKDOWN, PRODUCTION_NOTICES},
    measured_width, mount, node_count, nodes_with_id, style, texts, tree_of,
};
use crate::kit::markdown::{
    build::{parse_count, reset_parse_count},
    plain::{plain_parse_count, reset_plain_parse_count},
    plain_text,
};

fn cell_padding(app: &TestApp, cell: NodeId) -> LogicalPx {
    let padding = tree_of(app)
        .get(cell)
        .expect("the cell node is alive")
        .style()
        .inner()
        .padding;
    assert_eq!(padding.left, LengthPercentage::length(4.0));
    assert_eq!(padding.right, LengthPercentage::length(4.0));
    assert_eq!(padding.top, LengthPercentage::length(4.0));
    assert_eq!(padding.bottom, LengthPercentage::length(4.0));
    px(4.0)
}

fn assert_same_column_widths(widths: &[LogicalPx]) {
    assert_eq!(widths.len(), 4);
    assert!((widths[0] - widths[2]).abs() < px(0.01));
    assert!((widths[1] - widths[3]).abs() < px(0.01));
}

#[test]
fn gate_one_a_stable_frame_commits_nothing() {
    let (mut app, _node) = mount("# Title\n\n```rust\nlet x = 1;\n```");
    assert!(
        app.frame_record().rendered,
        "the mount frame must commit pixels"
    );
    let mut window = app.window();
    assert_eq!(
        window.drain().expect("the idle frame"),
        FrameOutcome::Idle,
        "a stable document must not re-render"
    );
    assert!(
        !window.frame_record().rendered,
        "an idle frame carries no rendered frame record"
    );
}

#[test]
fn gate_two_a_repaint_only_frame_shapes_and_lays_out_nothing() {
    let (mut app, _node) = mount("# Title\n\n**bold** `code` [link](u)\n\n- a\n- b");
    let first_text = app
        .frame_record()
        .stats
        .text
        .expect("the mount text pass report");
    let first_layout = app
        .frame_record()
        .stats
        .text_layout
        .expect("the mount text layout report");
    assert_eq!(first_text.shaped, 5, "the mount frame shapes five texts");
    assert_eq!(first_layout.laid, 6, "the mount frame lays out six texts");
    app.invalidate_all_lanes();
    let record = app.drain().expect("the repaint frame");
    assert!(record.rendered, "the repaint frame must commit");
    let text = record.stats.text.expect("the text pass report");
    let layout = record.stats.text_layout.expect("the text layout report");
    assert_eq!(text.shaped, 0, "a repaint-only frame reshapes no text");
    assert_eq!(layout.laid, 0, "a repaint-only frame relays out no text");
}

#[test]
fn gate_three_the_release_notes_stay_inside_the_node_budget() {
    let line_height = style().line_height();
    for source in [EN_RELEASE_NOTES, ZH_RELEASE_NOTES] {
        let (app, node) = mount(source);
        let total = node_count(&app, node.root());
        assert!(total <= 72, "the node budget is 72, measured {total}");
        assert_eq!(total, 70);
        let spacers = nodes_with_id(&app, node.root(), "MARKDOWN_SPACER");
        assert_eq!(spacers.len(), 34);
        for (index, spacer) in spacers.into_iter().enumerate() {
            let height = bounds(&app, spacer).size.height;
            assert!(
                (height - line_height).abs() < px(0.01),
                "{index}: {height} vs {line_height}"
            );
            let declared = tree_of(&app)
                .get(spacer)
                .expect("the spacer is alive")
                .style()
                .inner();
            assert_eq!(declared.flex_shrink, 0.0);
            assert_eq!(declared.flex_grow, 0.0);
        }
        let blocks = children(&app, node.root());
        assert_eq!(blocks.len(), 69);
        let text_blocks = blocks
            .iter()
            .filter(|block| {
                matches!(
                    tree_of(&app).get(**block).expect("block alive").kind(),
                    NodeKind::Text(_)
                )
            })
            .count();
        assert_eq!(text_blocks, 35);
        assert_eq!(1 + text_blocks + 34, total);
        let total_chars: usize = texts(&app, node.root())
            .iter()
            .map(|content| content.text.chars().count())
            .sum();
        assert_eq!(
            total_chars,
            if source == EN_RELEASE_NOTES {
                6806
            } else {
                2129
            }
        );
    }
}

#[test]
fn gate_four_the_nested_parse_count_follows_the_html_container_units() {
    reset_parse_count();
    let (_app, _node) = mount(EN_RELEASE_NOTES);
    assert_eq!(parse_count(), 1, "release notes need no nested parse");

    reset_parse_count();
    let (_app, _node) = mount(ZH_RELEASE_NOTES);
    assert_eq!(parse_count(), 1, "release notes need no nested parse");

    let notice = "<p align=\"center\">Delete the proxy \"Example Proxy\"? Network targets set to this proxy go back to the default proxy.</p>";
    reset_parse_count();
    let (_app, _node) = mount(notice);
    assert_eq!(
        parse_count(),
        2,
        "one html pair costs exactly one nested parse"
    );

    let hint = "Click the <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> button in the upper-right corner to select a widget.";
    reset_parse_count();
    let (_app, _node) = mount(hint);
    assert_eq!(parse_count(), 1, "an inline kbd costs no nested parse");

    let mixed = "<div>\n<p align=\"center\">a</p>\n<p>b</p>\n</div>";
    reset_parse_count();
    let (_app, _node) = mount(mixed);
    let mixed_parses = parse_count();
    assert_eq!(mixed_parses, 4);
    assert!(mixed_parses <= 1 + 3);
    assert!(mixed_parses <= 16);
}

#[test]
fn gate_four_every_production_rich_text_parses_exactly_once_or_once_nested() {
    let mut paired = 0;
    let mut inline = 0;
    for case in I18N_MARKDOWN {
        for source in [case.en, case.zh] {
            if source.is_empty() {
                continue;
            }
            reset_parse_count();
            let (_app, _node) = mount(source);
            let parses = parse_count();
            if source.contains("<p align") {
                assert_eq!(parses, 2, "{}", case.key);
                paired += 1;
            } else {
                assert_eq!(parses, 1, "{}", case.key);
                inline += 1;
            }
        }
    }
    assert_eq!(paired, 70);
    assert_eq!(inline, 5);
}

fn assert_no_legacy_ids(app: &TestApp, root: NodeId) {
    let tree = tree_of(app);
    let mut roots = 0;
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        if let Some(id) = tree.debug_id(current) {
            assert!(!id.starts_with("MARKDOWN_BLOCK"), "{id}");
            assert!(!id.starts_with("MARKDOWN_BLANK"), "{id}");
            if id == "MARKDOWN" {
                roots += 1;
            }
        }
        stack.extend(tree.get(current).expect("node alive").children());
    }
    assert_eq!(roots, 1, "exactly one markdown root");
}

#[test]
fn gate_five_no_legacy_block_or_blank_ids_survive() {
    let mut mounted = 0;
    for source in [EN_RELEASE_NOTES, ZH_RELEASE_NOTES] {
        let (app, node) = mount(source);
        assert_no_legacy_ids(&app, node.root());
        mounted += 1;
    }
    for case in I18N_MARKDOWN {
        for source in [case.en, case.zh] {
            if source.is_empty() {
                continue;
            }
            let (app, node) = mount(source);
            assert_no_legacy_ids(&app, node.root());
            mounted += 1;
        }
    }
    assert_eq!(mounted, 77);
    let (app, node) = mount(
        "# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> q\n\n- [x] t\n\n```\ncode\n```\n\n---\n\n<div>\n<p align=\"center\">x</p>\n</div>",
    );
    assert_eq!(count(&app, "MARKDOWN"), 1);
    assert_no_legacy_ids(&app, node.root());
}

#[test]
fn gate_six_the_markdown_subtree_holds_no_dependency_edges() {
    let (mut app, node) = mount(
        "# Title\n\n**bold**\n\n- a\n- b\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> q\n\n```\ncode\n```",
    );
    let window = app.window_id();
    let blocks = children(&app, node.root());
    assert_eq!(blocks.len(), 11);
    for child in blocks {
        let removed = app
            .app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .remove(child)
            .expect("remove the markdown subtree");
        assert!(removed.is_empty(), "a markdown subtree holds no bind_*");
    }
    let root_edges = app
        .app_mut()
        .window_mut(window)
        .expect("window")
        .tree_mut()
        .remove(node.root())
        .expect("remove the markdown root");
    assert_eq!(root_edges.len(), 1);
    assert_eq!(root_edges[0].0, node.root());
    assert_eq!(root_edges[0].1.len(), 1);
}

#[test]
fn the_blank_line_spacer_is_flex_fixed_at_one_line_height() {
    let (app, node) = mount("a\n\nb");
    let spacers = nodes_with_id(&app, node.root(), "MARKDOWN_SPACER");
    assert_eq!(spacers.len(), 1);
    let spacer = spacers[0];
    let declared = tree_of(&app)
        .get(spacer)
        .expect("the spacer is alive")
        .style()
        .inner();
    assert_eq!(declared.flex_shrink, 0.0);
    assert_eq!(declared.flex_grow, 0.0);
    assert_eq!(
        declared.size.height,
        taffy::style::Dimension::length(style().line_height().value())
    );
    assert_eq!(bounds(&app, spacer).size.height, style().line_height());
}

#[test]
fn the_i18n_markdown_fixture_renders_without_residual_source_markup() {
    assert_eq!(I18N_MARKDOWN.len(), 39);
    let mut en_rich = 0;
    let mut zh_rich = 0;
    for case in I18N_MARKDOWN {
        for (language, source, rich, expected) in [
            ("en", case.en, case.en_rich, case.en_visible),
            ("zh", case.zh, case.zh_rich, case.zh_visible),
        ] {
            if !rich {
                assert!(source.is_empty(), "{}:{language}", case.key);
                continue;
            }
            if language == "en" {
                en_rich += 1;
            } else {
                zh_rich += 1;
            }
            let (app, node) = mount(source);
            let visible = texts(&app, node.root())
                .iter()
                .map(|content| content.text.as_str())
                .collect::<Vec<&str>>()
                .join("\n");
            assert_eq!(visible, expected, "{}:{language}", case.key);
            for marker in ["<p", "<kbd>", "![](", "**", "`"] {
                assert!(
                    !visible.contains(marker),
                    "{}:{language} leaks the source marker {marker}: {visible}",
                    case.key
                );
            }
        }
    }
    assert_eq!(en_rich, 36);
    assert_eq!(zh_rich, 39);
    assert_eq!(en_rich + zh_rich, 75);
}

#[test]
fn the_production_notices_render_their_frozen_visible_text() {
    assert_eq!(PRODUCTION_NOTICES.len(), 21);
    for key in PRODUCTION_NOTICES {
        let case = I18N_MARKDOWN
            .iter()
            .find(|case| case.key == *key)
            .unwrap_or_else(|| panic!("{key} is not in the fixture"));
        for (language, source, rich, expected) in [
            ("en", case.en, case.en_rich, case.en_visible),
            ("zh", case.zh, case.zh_rich, case.zh_visible),
        ] {
            assert!(rich, "{key}:{language} is a rich notice");
            let (app, node) = mount(source);
            let visible = texts(&app, node.root())
                .iter()
                .map(|content| content.text.as_str())
                .collect::<Vec<&str>>()
                .join("\n");
            assert_eq!(visible, expected, "{key}:{language}");
        }
    }
}

#[test]
fn the_zh_release_notes_merge_fifteen_soft_breaks_into_eleven_paragraphs() {
    let (app, node) = mount(ZH_RELEASE_NOTES);
    let contents = texts(&app, node.root());
    assert_eq!(contents.len(), 35);
    let multiline = contents
        .iter()
        .filter(|content| content.text.contains('\n'))
        .count();
    assert_eq!(multiline, 11);
    let breaks: usize = contents
        .iter()
        .map(|content| {
            content
                .text
                .chars()
                .filter(|character| *character == '\n')
                .count()
        })
        .sum();
    assert_eq!(breaks, 15);
    assert_eq!(node_count(&app, node.root()), 70);
}

#[test]
fn gate_seven_table_columns_take_their_measured_natural_widths() {
    let source = "| short | a very long piece of cell text here |\n|---|---|\n| s | t |";
    let (mut app, node) = mount(source);
    let widths = cell_widths(&app, node.root());
    assert_same_column_widths(&widths);
    assert!(widths[1] > widths[0]);
    let short = measured_width(&mut app, "short", true);
    let long = measured_width(&mut app, "a very long piece of cell text here", true);
    assert!(
        short + long + px(16.0) < px(800.0),
        "the table fits the available width"
    );
    let cells = nodes_with_id(&app, node.root(), "MARKDOWN_TABLE_CELL");
    let padding = cell_padding(&app, cells[0]) + cell_padding(&app, cells[1]);
    assert!((widths[0] - padding - short).abs() < px(1.0));
    assert!((widths[1] - padding - long).abs() < px(1.0));
    let ratio = widths[1] / widths[0];
    let expected = (long + padding) / (short + padding);
    assert!(((ratio - expected) / expected).abs() < 0.05);

    let left = "x".repeat(100);
    let right = "y".repeat(110);
    let source = format!("| {left} | {right} |\n|---|---|\n| a | b |");
    let (mut app, node) = mount(&source);
    let widths = cell_widths(&app, node.root());
    assert_same_column_widths(&widths);
    assert!(widths[1] > widths[0]);
    assert!(widths[0] + widths[1] <= px(800.5));
    let left_natural = measured_width(&mut app, &left, true);
    let right_natural = measured_width(&mut app, &right, true);
    let ratio = widths[1] / widths[0];
    let expected = (right_natural + px(8.0)) / (left_natural + px(8.0));
    assert!(((ratio - expected) / expected).abs() < 0.05);
    assert!(
        widths[0] < left_natural + px(8.0) - px(1.0),
        "the shrunk column is narrower than its natural width"
    );

    let source = "<div>\n| a | b |\n|---|---|\n| 1 | long cell text |\n</div>";
    let (mut app, node) = mount(source);
    let widths = cell_widths(&app, node.root());
    assert_same_column_widths(&widths);
    assert!(widths[1] > widths[0]);
    let long = measured_width(&mut app, "long cell text", false);
    assert!((widths[1] - px(8.0) - long).abs() < px(1.0));
}

#[test]
fn gate_four_plain_text_counts_the_nested_html_parse() {
    reset_plain_parse_count();
    let _ = plain_text(EN_RELEASE_NOTES);
    assert_eq!(plain_parse_count(), 1);

    reset_plain_parse_count();
    let _ = plain_text("<p align=\"center\">x</p>");
    assert_eq!(plain_parse_count(), 2);
}

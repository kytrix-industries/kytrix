use ui_core::{
    node::{
        id::NodeId,
        tree::{NodeKind, NodeTree},
    },
    text::{TextAlign, TextContent},
};

use super::{TestApp, children, style, tree_of};

fn align_of(alignment: TextAlign) -> &'static str {
    match alignment {
        TextAlign::Center => "center",
        TextAlign::Right => "right",
        _ => "left",
    }
}

fn heading_level(content: &TextContent) -> Option<u8> {
    if content.style.weight != 700 {
        return None;
    }
    let ratio = content.style.size.value() / style().body_size.value();
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

fn push_line(lines: &mut Vec<String>, depth: usize, line: String) {
    let mut rendered = String::with_capacity(depth * 2 + line.len());
    for _ in 0..depth {
        rendered.push_str("  ");
    }
    rendered.push_str(&line);
    lines.push(rendered);
}

fn marker_text(tree: &NodeTree, item: NodeId) -> Option<String> {
    for child in tree.get(item).ok()?.children() {
        if tree.debug_id(*child) == Some("MARKDOWN_LIST_MARKER")
            && let NodeKind::Text(content) = tree.get(*child).ok()?.kind()
        {
            return Some(content.text.to_string());
        }
    }
    None
}

fn is_ordered(tree: &NodeTree, list: NodeId) -> bool {
    tree.get(list)
        .ok()
        .and_then(|entry| {
            entry
                .children()
                .iter()
                .find_map(|item| marker_text(tree, *item))
        })
        .is_some_and(|marker| !marker.ends_with('•'))
}

fn ordered_start(tree: &NodeTree, list: NodeId) -> u32 {
    tree.get(list)
        .ok()
        .and_then(|entry| {
            entry
                .children()
                .iter()
                .find_map(|item| marker_text(tree, *item))
        })
        .and_then(|marker| marker.trim_end_matches('.').parse().ok())
        .unwrap_or(1)
}

fn item_is_loose(tree: &NodeTree, item: NodeId) -> bool {
    tree.get(item).ok().is_some_and(|entry| {
        entry
            .children()
            .iter()
            .any(|child| tree.debug_id(*child) == Some("MARKDOWN_LIST_CONTENT"))
    })
}

fn push_text(lines: &mut Vec<String>, depth: usize, content: &TextContent) {
    let runs = content.runs().len();
    let align = align_of(content.style.alignment);
    match heading_level(content) {
        Some(level) => push_line(
            lines,
            depth,
            format!("h{level}[align={align}, runs={runs}]"),
        ),
        None => push_line(lines, depth, format!("p[align={align}, runs={runs}]")),
    }
    for inline_box in &content.inline_boxes {
        if inline_box.paint.is_some() {
            push_line(
                lines,
                depth + 1,
                format!(
                    "image[{}×{}]",
                    inline_box.size.width.value(),
                    inline_box.size.height.value()
                ),
            );
        }
    }
}

fn push_node(lines: &mut Vec<String>, depth: usize, app: &TestApp, node: NodeId) {
    let tree = tree_of(app);
    let entry = tree.get(node).expect("the outline node");
    let id = tree.debug_id(node);
    match entry.kind() {
        NodeKind::Text(content) => {
            if id == Some("MARKDOWN_CODE") {
                push_line(lines, depth, "code[bg=1, mono=1]".to_owned());
            } else {
                push_text(lines, depth, content);
            }
        }
        NodeKind::Box => match id {
            Some("MARKDOWN_QUOTE") => {
                push_line(lines, depth, "quote[]".to_owned());
                for child in entry.children() {
                    if tree.debug_id(*child) == Some("MARKDOWN_QUOTE_BODY") {
                        for block in tree.get(*child).expect("the quote body").children() {
                            push_node(lines, depth + 1, app, *block);
                        }
                    }
                }
            }
            Some("MARKDOWN_LIST") => {
                if is_ordered(tree, node) {
                    push_line(
                        lines,
                        depth,
                        format!("list[ordered, start={}]", ordered_start(tree, node)),
                    );
                } else {
                    push_line(lines, depth, "list[bullet]".to_owned());
                }
                for item in entry.children() {
                    if tree.debug_id(*item) != Some("MARKDOWN_LIST_ITEM") {
                        continue;
                    }
                    let tightness = if item_is_loose(tree, *item) {
                        "loose"
                    } else {
                        "tight"
                    };
                    push_line(lines, depth + 1, format!("item[{tightness}]"));
                    for child in tree.get(*item).expect("the item node").children() {
                        match tree.debug_id(*child) {
                            Some("MARKDOWN_LIST_MARKER") => {}
                            Some("MARKDOWN_LIST_CONTENT") => {
                                for block in tree.get(*child).expect("the content").children() {
                                    push_node(lines, depth + 2, app, *block);
                                }
                            }
                            _ => push_node(lines, depth + 2, app, *child),
                        }
                    }
                }
            }
            Some("MARKDOWN_TABLE") => {
                let rows: Vec<NodeId> = entry
                    .children()
                    .iter()
                    .copied()
                    .filter(|child| tree.debug_id(*child) == Some("MARKDOWN_TABLE_ROW"))
                    .collect();
                let columns = rows
                    .first()
                    .map(|row| {
                        tree.get(*row)
                            .expect("the row")
                            .children()
                            .iter()
                            .filter(|cell| tree.debug_id(**cell) == Some("MARKDOWN_TABLE_CELL"))
                            .count()
                    })
                    .unwrap_or(0);
                push_line(lines, depth, format!("table[cols={columns}]"));
                for row in &rows {
                    let header = tree
                        .get(*row)
                        .expect("the row")
                        .children()
                        .iter()
                        .filter(|cell| tree.debug_id(**cell) == Some("MARKDOWN_TABLE_CELL"))
                        .filter_map(|cell| {
                            tree.get(*cell)
                                .expect("the cell")
                                .children()
                                .iter()
                                .find_map(|child| match tree.get(*child).ok()?.kind() {
                                    NodeKind::Text(content) => Some(content.style.weight == 700),
                                    _ => None,
                                })
                        })
                        .next()
                        .unwrap_or(false);
                    push_line(
                        lines,
                        depth + 1,
                        if header {
                            "row[header]".to_owned()
                        } else {
                            "row[body]".to_owned()
                        },
                    );
                    for cell in tree.get(*row).expect("the row").children() {
                        if tree.debug_id(*cell) != Some("MARKDOWN_TABLE_CELL") {
                            continue;
                        }
                        let text = tree
                            .get(*cell)
                            .expect("the cell")
                            .children()
                            .iter()
                            .find_map(|child| match tree.get(*child).ok()?.kind() {
                                NodeKind::Text(content) => Some(content.as_ref()),
                                _ => None,
                            });
                        let width = tree
                            .get(*cell)
                            .expect("the cell")
                            .layout()
                            .bounds
                            .size
                            .width;
                        push_line(
                            lines,
                            depth + 2,
                            format!(
                                "cell[align={}, w={}, has_text={}]",
                                text.map(|content| align_of(content.style.alignment))
                                    .unwrap_or("left"),
                                width.value().round() as i64,
                                u8::from(text.is_some())
                            ),
                        );
                        for block in tree.get(*cell).expect("the cell").children() {
                            if matches!(
                                tree.get(*block).expect("the cell child").kind(),
                                NodeKind::Text(_)
                            ) {
                                continue;
                            }
                            push_node(lines, depth + 3, app, *block);
                        }
                    }
                }
            }
            Some("MARKDOWN_RULE" | "MARKDOWN_TABLE_RULE") => {
                push_line(lines, depth, "rule[]".to_owned())
            }
            Some("MARKDOWN_SPACER") => {
                let height = entry.layout().bounds.size.height.value();
                push_line(lines, depth, format!("spacer[h={height:.1}]"));
            }
            _ => {
                push_line(lines, depth, format!("box[id={}]", id.unwrap_or("?")));
                for child in entry.children() {
                    push_node(lines, depth + 1, app, *child);
                }
            }
        },
        _ => {}
    }
}

pub(super) fn outline(app: &TestApp, root: NodeId) -> Vec<String> {
    let mut lines = Vec::new();
    for child in children(app, root) {
        push_node(&mut lines, 0, app, child);
    }
    lines
}

#[test]
fn the_outline_formats_every_kind_and_attribute() {
    let (app, node) = super::mount(
        "# H\n\ntext **b** ![a](file://icons/x.png)\n\n- a\n\n- b\n\n> q\n\n```\ncode\n```\n\n---",
    );
    let lines = outline(&app, node.root());
    assert_eq!(
        lines,
        vec![
            "h1[align=left, runs=0]",
            "spacer[h=19.6]",
            "p[align=left, runs=1]",
            "  image[16×16]",
            "spacer[h=19.6]",
            "list[bullet]",
            "  item[loose]",
            "    p[align=left, runs=0]",
            "  item[loose]",
            "    p[align=left, runs=0]",
            "spacer[h=19.6]",
            "quote[]",
            "  p[align=left, runs=0]",
            "spacer[h=19.6]",
            "code[bg=1, mono=1]",
            "spacer[h=19.6]",
            "rule[]",
        ]
    );
}

#[test]
fn the_outline_classifies_tables_cells_and_task_items() {
    let (app, node) = super::mount("| h1 | h2 |\n|:--|--:|\n| 1 | 2 |");
    assert_eq!(
        outline(&app, node.root()),
        vec![
            "table[cols=2]",
            "  row[header]",
            "    cell[align=left, w=23, has_text=1]",
            "    cell[align=right, w=24, has_text=1]",
            "  row[body]",
            "    cell[align=left, w=23, has_text=1]",
            "    cell[align=right, w=24, has_text=1]",
        ]
    );

    let (app, node) = super::mount("- [x] done\n- plain");
    let lines = outline(&app, node.root());
    assert_eq!(
        lines,
        vec![
            "list[bullet]",
            "  item[tight]",
            "    p[align=left, runs=0]",
            "  item[tight]",
            "    p[align=left, runs=0]",
        ]
    );

    let (app, node) = super::mount("3. x\n4. y");
    let lines = outline(&app, node.root());
    assert_eq!(lines[0], "list[ordered, start=3]");
}

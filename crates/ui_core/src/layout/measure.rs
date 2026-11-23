use std::cell::Cell;

use taffy::{LayoutInput, LayoutOutput, Size as TaffySize};

use crate::{
    layout::Axis,
    math::{LogicalPx, LogicalSize},
    node::{id::NodeId, tree::NodeTree},
};

pub(crate) fn compute_leaf(tree: &mut NodeTree, node: NodeId, inputs: LayoutInput) -> LayoutOutput {
    let violation: Cell<Option<(Axis, LogicalPx)>> = Cell::new(None);

    let output = {
        let style = tree.taffy_style(node);
        let measure = tree.measure_of(node);
        taffy::compute_leaf_layout(
            inputs,
            style,
            |_ptr, _basis| 0.0,
            |known, available| {
                let size = match measure {
                    Some(measure) => measure(known, available),
                    None => LogicalSize::ZERO,
                };
                let width_ok = size.width.is_finite() && size.width >= LogicalPx::ZERO;
                let height_ok = size.height.is_finite() && size.height >= LogicalPx::ZERO;
                if !width_ok {
                    violation.set(Some((Axis::X, size.width)));
                } else if !height_ok {
                    violation.set(Some((Axis::Y, size.height)));
                }
                TaffySize {
                    width: if width_ok { size.width.value() } else { 0.0 },
                    height: if height_ok { size.height.value() } else { 0.0 },
                }
            },
        )
    };

    if let Some((axis, value)) = violation.get() {
        tree.note_invalid_measure(node, axis, value);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crate::{
        error::EngineError,
        layout::{Axis, LayoutReport, MeasureFn},
        math::{LogicalPx, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::{style::Style, style_builder::StyleBuilder},
    };

    fn fixed(width: LogicalPx, height: LogicalPx) -> Style {
        StyleBuilder::new().size(width, height).build()
    }

    fn block(width: LogicalPx, height: LogicalPx) -> Style {
        let mut style = fixed(width, height);
        style.display(taffy::style::Display::Block);
        style
    }

    fn add(tree: &mut NodeTree, parent: NodeId, style: Style) -> NodeId {
        NodeBuilder::boxed()
            .style(style)
            .build(tree, parent)
            .expect("挂载成功")
    }

    fn layout(tree: &mut NodeTree) -> Result<LayoutReport, EngineError> {
        tree.compute_layout(taffy::geometry::Size {
            width: Some(100.0),
            height: Some(100.0),
        })
    }

    fn bounds(tree: &NodeTree, node: NodeId) -> LogicalRect {
        tree.get(node).expect("存在").layout().bounds
    }

    #[test]
    fn measure_fn_is_boxed_fn() {
        let measure: MeasureFn = Box::new(|_known, _available| LogicalSize::new(px(1.0), px(1.0)));
        let available = taffy::geometry::Size {
            width: taffy::AvailableSpace::MaxContent,
            height: taffy::AvailableSpace::MaxContent,
        };
        assert_eq!(
            measure(taffy::geometry::Size::NONE, available),
            LogicalSize::new(px(1.0), px(1.0))
        );
    }

    #[test]
    fn leaf_without_measure_is_zero_size() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(crate::paint::canvas::Canvas::new(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(1.0),
            px(1.0),
        )))
        .build(&mut tree, root)
        .expect("挂载成功");

        let _ = layout(&mut tree).expect("布局成功");

        assert!(
            bounds(&tree, canvas).is_empty(),
            "无 measure 的空叶子尺寸为 0"
        );
    }

    #[test]
    fn fixed_style_leaf_does_not_call_measure() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let node = NodeBuilder::boxed()
            .style(fixed(px(20.0), px(20.0)))
            .measure(Box::new(move |_known, _available| {
                counter.set(counter.get() + 1);
                LogicalSize::new(px(1.0), px(1.0))
            }))
            .build(&mut tree, root)
            .expect("挂载成功");

        let inputs = taffy::LayoutInput {
            run_mode: taffy::RunMode::ComputeSize,
            sizing_mode: taffy::SizingMode::InherentSize,
            axis: taffy::RequestedAxis::Both,
            known_dimensions: taffy::geometry::Size {
                width: Some(20.0),
                height: Some(20.0),
            },
            known_dimensions_are_definite: taffy::geometry::Size {
                width: true,
                height: true,
            },
            parent_size: taffy::geometry::Size::NONE,
            available_space: taffy::geometry::Size {
                width: taffy::AvailableSpace::Definite(100.0),
                height: taffy::AvailableSpace::Definite(100.0),
            },
            vertical_margins_are_collapsible: taffy::geometry::Line::FALSE,
        };
        let output = super::compute_leaf(&mut tree, node, inputs);

        assert_eq!(output.size.width, 20.0);
        assert_eq!(output.size.height, 20.0);
        assert_eq!(calls.get(), 0, "两侧尺寸已知 ⇒ taffy 早退，不调 measure");
    }

    #[test]
    fn measure_receives_definite_width_from_block() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = add(&mut tree, root, block(px(100.0), px(100.0)));
        let observed = Rc::new(Cell::new(f32::NAN));
        let seen = observed.clone();
        let node = NodeBuilder::boxed()
            .measure(Box::new(move |_known, available| {
                let width = available.width.into_option().unwrap_or(f32::NAN);
                seen.set(width);
                LogicalSize::new(px(width.min(50.0)), px(10.0))
            }))
            .build(&mut tree, container)
            .expect("挂载成功");

        let _ = layout(&mut tree).expect("布局成功");

        assert_eq!(observed.get(), 100.0, "block 容器把确定宽度传给 measure");
        assert_eq!(bounds(&tree, node).size.width, px(100.0));
        assert_eq!(
            bounds(&tree, node).size.height,
            px(10.0),
            "高度来自 measure"
        );
    }

    #[test]
    fn measure_records_all_available_space_variants() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
        let record = seen.clone();
        let row = {
            let mut style = fixed(px(200.0), px(100.0));
            style.align_items(taffy::style::AlignItems::FLEX_START);
            add(&mut tree, root, style)
        };
        let _node = NodeBuilder::boxed()
            .measure(Box::new(move |_known, available| {
                record.borrow_mut().push(available.width);
                LogicalSize::new(px(80.0), px(10.0))
            }))
            .build(&mut tree, row)
            .expect("挂载成功");

        let _ = tree
            .compute_layout(taffy::geometry::Size {
                width: Some(200.0),
                height: Some(100.0),
            })
            .expect("布局成功");

        let seen = seen.borrow();
        assert!(!seen.is_empty(), "measure 必须被调用");
        assert!(
            seen.iter().any(|space| space.is_definite()),
            "必须至少收到一次 Definite：{seen:?}"
        );
        assert!(
            seen.iter()
                .any(|space| matches!(space, taffy::AvailableSpace::MaxContent)),
            "固有尺寸测量必须收到 MaxContent：{seen:?}"
        );
    }

    #[test]
    fn an_invalid_measure_is_replaced_with_zero_before_it_reaches_taffy() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .measure(Box::new(|_known, _available| {
                LogicalSize::new(px(f32::NAN), px(f32::INFINITY))
            }))
            .build(&mut tree, root)
            .expect("挂载成功");

        let output = super::compute_leaf(
            &mut tree,
            node,
            taffy::LayoutInput {
                run_mode: taffy::RunMode::PerformLayout,
                sizing_mode: taffy::SizingMode::ContentSize,
                axis: taffy::RequestedAxis::Both,
                known_dimensions: taffy::geometry::Size::NONE,
                known_dimensions_are_definite: taffy::geometry::Size {
                    width: false,
                    height: false,
                },
                parent_size: taffy::geometry::Size::NONE,
                available_space: taffy::geometry::Size {
                    width: taffy::AvailableSpace::Definite(100.0),
                    height: taffy::AvailableSpace::Definite(100.0),
                },
                vertical_margins_are_collapsible: taffy::geometry::Line::FALSE,
            },
        );

        assert_eq!(
            output.size,
            taffy::geometry::Size {
                width: 0.0,
                height: 0.0
            },
            "两个轴的违规值都必须替换成 0，不得把 NaN/∞ 交给 taffy"
        );
        assert!(
            matches!(
                tree.take_invalid_measure(),
                Some(EngineError::InvalidMeasure { axis: Axis::X, .. })
            ),
            "首条违规仍然要如实上报（X 先于 Y）"
        );
    }

    #[test]
    fn nan_width_is_invalid_measure_x() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .measure(Box::new(|_known, _available| {
                LogicalSize::new(px(f32::NAN), px(10.0))
            }))
            .build(&mut tree, root)
            .expect("挂载成功");

        let error = layout(&mut tree).expect_err("NaN 必须报错");

        assert!(matches!(
            error,
            EngineError::InvalidMeasure {
                node: reported,
                axis: Axis::X,
                value,
            } if reported == node && value.value().is_nan()
        ));
    }

    #[test]
    fn negative_height_is_invalid_measure_y() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .measure(Box::new(|_known, _available| {
                LogicalSize::new(px(10.0), px(-1.0))
            }))
            .build(&mut tree, root)
            .expect("挂载成功");

        let error = layout(&mut tree).expect_err("负尺寸必须报错");

        match error {
            EngineError::InvalidMeasure {
                node: reported,
                axis,
                value,
            } => {
                assert_eq!(reported, node);
                assert_eq!(axis, Axis::Y);
                assert_eq!(value, px(-1.0));
            }
            other => panic!("expected InvalidMeasure, got {other:?}"),
        }
    }

    #[test]
    fn first_violation_wins() {
        fn run() -> EngineError {
            let mut tree = NodeTree::new();
            let root = tree.root();
            let first = NodeBuilder::boxed()
                .measure(Box::new(|_known, _available| {
                    LogicalSize::new(px(f32::NAN), px(1.0))
                }))
                .build(&mut tree, root)
                .expect("挂载成功");
            let _second = NodeBuilder::boxed()
                .measure(Box::new(|_known, _available| {
                    LogicalSize::new(px(1.0), px(f32::INFINITY))
                }))
                .build(&mut tree, root)
                .expect("挂载成功");

            let error = layout(&mut tree).expect_err("必须报错");
            match error {
                EngineError::InvalidMeasure { node, .. } => {
                    assert_eq!(node, first, "取求值序靠前的节点");
                }
                other => panic!("expected InvalidMeasure, got {other:?}"),
            }
            error
        }

        let first_run = run();
        let second_run = run();
        assert!(
            matches!(first_run, EngineError::InvalidMeasure { .. })
                && matches!(second_run, EngineError::InvalidMeasure { .. }),
            "两次都必须是 InvalidMeasure"
        );
    }

    #[test]
    fn invalid_measure_leaves_empty_boxes() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .measure(Box::new(|_known, _available| {
                LogicalSize::new(px(f32::NAN), px(f32::NAN))
            }))
            .build(&mut tree, root)
            .expect("挂载成功");

        let _ = layout(&mut tree).expect_err("必须报错");

        assert!(
            bounds(&tree, node).is_empty(),
            "NaN 尺寸的布局盒必须为空（hit/damage 因此安全）"
        );
    }

    #[test]
    fn invalidate_measure_forces_remeasure() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let node = NodeBuilder::boxed()
            .measure(Box::new(move |_known, _available| {
                counter.set(counter.get() + 1);
                LogicalSize::new(px(40.0), px(10.0))
            }))
            .build(&mut tree, root)
            .expect("挂载成功");
        let _ = layout(&mut tree).expect("布局成功");
        let after_first = calls.get();
        assert!(after_first > 0);

        let _ = layout(&mut tree).expect("布局成功");
        assert_eq!(calls.get(), after_first, "输入不变 ⇒ 完全命中缓存");

        tree.invalidate_measure(node).expect("节点存活");
        let _ = layout(&mut tree).expect("布局成功");

        assert!(
            calls.get() > after_first,
            "invalidate_measure 之后必须重新测量（{} → {}）",
            after_first,
            calls.get()
        );
        assert!(matches!(
            tree.invalidate_measure(crate::node::id::NodeId::for_test(99, 1)),
            Err(EngineError::StaleNode { .. })
        ));
    }

    #[test]
    fn text_measure_matches_the_committed_layout_for_all_three_available_spaces() {
        use taffy::AvailableSpace;

        use crate::text::{TextContent, TextOverflow, TextStyle};

        let mut text = crate::text::context::TextContext::new();
        let style = TextStyle::new(px(16.0)).with_overflow(TextOverflow::Ellipsis, 2);
        let content = TextContent::new(
            smol_str::SmolStr::new_static("lorem ipsum dolor sit amet consectetur adipiscing elit"),
            style,
        );
        let (shaped, _) = text.shape_cached(&content);

        let min_content = shaped.measure(
            taffy::Size {
                width: None,
                height: None,
            },
            taffy::Size {
                width: AvailableSpace::MinContent,
                height: AvailableSpace::MaxContent,
            },
        );
        let max_content = shaped.measure(
            taffy::Size {
                width: None,
                height: None,
            },
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );
        assert!(
            min_content.width <= max_content.width,
            "MinContent 宽度必须 ≤ MaxContent：{min_content:?} vs {max_content:?}"
        );
        assert!(
            min_content.height >= max_content.height,
            "min-content 会换更多行（被 max_lines=2 截断后仍 ≥ 单行高度）：{min_content:?} vs {max_content:?}"
        );

        let width = ((min_content.width + max_content.width) / 2.0).max(min_content.width);
        let measured = shaped.measure(
            taffy::Size {
                width: None,
                height: None,
            },
            taffy::Size {
                width: AvailableSpace::Definite(width.value()),
                height: AvailableSpace::MaxContent,
            },
        );
        assert!(
            measured.width <= width + 0.001,
            "Definite(w) 的测量宽不得超过可用宽：{measured:?}（w={width}）"
        );

        let measure_shaped = Rc::clone(&shaped);
        let mut tree = NodeTree::new();
        let root = tree.root();
        {
            let mut root_style = fixed(px(400.0), px(400.0));
            root_style
                .align_items(taffy::style::AlignItems::FLEX_START)
                .flex_direction(taffy::style::FlexDirection::Column);
            tree.set_style(root, root_style).expect("root 存活");
        }
        let mut node_style = fixed(width, px(0.0));
        node_style.height(taffy::style::Dimension::auto());
        let node = NodeBuilder::boxed()
            .style(node_style)
            .measure(Box::new(move |known, available| {
                measure_shaped.measure(known, available)
            }))
            .build(&mut tree, root)
            .expect("挂载成功");

        let _ = tree
            .compute_layout(taffy::geometry::Size {
                width: Some(400.0),
                height: Some(400.0),
            })
            .expect("布局成功");

        let committed = bounds(&tree, node);
        assert_eq!(
            committed.size.width, width,
            "固定宽度节点的提交宽度 = 样式宽（measure 只决定内容尺寸）"
        );
        assert_eq!(
            committed.size.height, measured.height,
            "提交高度必须等于 Definite(内容宽) 的测量高度：测的和排的是同一次断行 + 同一份 max_lines 截断"
        );
        assert!(
            measured.height >= max_content.height,
            "定宽断行后的可见高度不得小于单行高度（max_lines 截断只会截掉多余行）：{measured:?}"
        );
    }
}

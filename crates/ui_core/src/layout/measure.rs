//! measure：自定义叶子测量的求值 + 违规检测。
//!
//! **没有**独立的 `(measure_version, available_space)` 缓存：taffy `Cache` 的 key 已经是
//! `(known_dimensions, available_space, sizing_mode, run_mode, parent_size, mode)`
//! （`taffy-0.15.0/src/tree/cache.rs` 的 `CacheKey::from`），再建一份就是双份真相（AGENTS §1.2/§1.4）。
//! 「闭包捕获的状态变了」由 [`NodeTree::invalidate_measure`] 表达的**失效**（清 node..root 的缓存），
//! 不是一个版本号字段（没有读者的字段按 AGENTS §2.1 不加）。
//!
//! 设计见 `design/phase-04-layout.md` §9。

use std::cell::Cell;

use taffy::{LayoutInput, LayoutOutput, Size as TaffySize};

use crate::{
    layout::Axis,
    math::Size,
    node::{id::NodeId, tree::NodeTree},
};

/// 求值一个叶子节点：taffy 的 `compute_leaf_layout`（padding/border/aspect-ratio/min-max 全由它处理）
/// + 引擎的 [`crate::layout::MeasureFn`]。
///
/// 违规检测做在**闭包的原始返回值**上：`compute_leaf_layout` 之后会 min/max clamp 并加 padding/border
/// （`compute/leaf.rs:148-156`），NaN 可能被吞掉或被放大，只有原始值能定位到应用代码的错误。
pub(crate) fn compute_leaf(tree: &mut NodeTree, node: NodeId, inputs: LayoutInput) -> LayoutOutput {
    // 首条违规（按 taffy 的求值顺序）；`Cell` 让闭包在共享借用下记录。
    let violation: Cell<Option<(Axis, f32)>> = Cell::new(None);

    let output = {
        let style = tree.taffy_style(node);
        let measure = tree.measure_of(node);
        taffy::compute_leaf_layout(
            inputs,
            style,
            // v1 不解析 `calc()`：`Calc` 是不透明指针（`style/dimension.rs:106-109`），taffy 只提供
            // `calc_value() -> *const ()`，没有公开的求值器；它自带的 `TaffyTree` 同样返回 0.0。
            |_ptr, _basis| 0.0,
            |known, available| {
                let size = match measure {
                    Some(measure) => measure(known, available),
                    // 无 measure 的空叶子：0×0（与 taffy `TaffyTree::compute_layout` 的默认闭包一致）
                    None => Size::ZERO,
                };
                if !size.width.is_finite() || size.width < 0.0 {
                    violation.set(Some((Axis::X, size.width)));
                } else if !size.height.is_finite() || size.height < 0.0 {
                    violation.set(Some((Axis::Y, size.height)));
                }
                TaffySize {
                    width: size.width,
                    height: size.height,
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

    use taffy::style::Dimension;

    use crate::{
        error::EngineError,
        layout::{Axis, LayoutReport, MeasureFn},
        math::{Rect, Size},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::style::{Clip, Style},
    };

    fn fixed(width: f32, height: f32) -> Style {
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: Dimension::length(width),
            height: Dimension::length(height),
        };
        Style {
            inner,
            clip: Clip::None,
        }
    }

    fn block(width: f32, height: f32) -> Style {
        let mut style = fixed(width, height);
        style.inner.display = taffy::style::Display::Block;
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

    fn bounds(tree: &NodeTree, node: NodeId) -> Rect {
        tree.get(node).expect("存在").layout().bounds
    }

    #[test]
    fn measure_fn_is_boxed_fn() {
        // 类型即契约：必须是 `Fn`（不是 `FnMut`），两个参数分别是已知尺寸与可用空间。
        let measure: MeasureFn = Box::new(|_known, _available| Size::new(1.0, 1.0));
        let available = taffy::geometry::Size {
            width: taffy::AvailableSpace::MaxContent,
            height: taffy::AvailableSpace::MaxContent,
        };
        assert_eq!(
            measure(taffy::geometry::Size::NONE, available),
            Size::new(1.0, 1.0)
        );
    }

    #[test]
    fn leaf_without_measure_is_zero_size() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(crate::paint::canvas::Canvas::new(Rect::new(
            0.0, 0.0, 1.0, 1.0,
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
        // 两侧 `known_dimensions` 都已知（`RunMode::ComputeSize`）时 taffy 直接早退，不调 measure。
        // 布局路径上父算法（flex）的第一趟会先用 Min/MaxContent 探固有尺寸，那是**应该**调的，
        // 所以这里用 `compute_leaf` 的单元口径固定该早退语义（与 taffy `compute/leaf.rs:98-115` 一致）。
        let mut tree = NodeTree::new();
        let root = tree.root();
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let node = NodeBuilder::boxed()
            .style(fixed(20.0, 20.0))
            .measure(Box::new(move |_known, _available| {
                counter.set(counter.get() + 1);
                Size::new(1.0, 1.0)
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
        let container = add(&mut tree, root, block(100.0, 100.0));
        let observed = Rc::new(Cell::new(f32::NAN));
        let seen = observed.clone();
        let node = NodeBuilder::boxed()
            .measure(Box::new(move |_known, available| {
                let width = available.width.into_option().unwrap_or(f32::NAN);
                seen.set(width);
                Size::new(width.min(50.0), 10.0)
            }))
            .build(&mut tree, container)
            .expect("挂载成功");

        let _ = layout(&mut tree).expect("布局成功");

        assert_eq!(observed.get(), 100.0, "block 容器把确定宽度传给 measure");
        // block 布局里 auto 宽度子节点会撑满包含块 ⇒ 布局盒 100 宽，measure 宽度只用于固有尺寸。
        assert_eq!(bounds(&tree, node).size.width, 100.0);
        assert_eq!(bounds(&tree, node).size.height, 10.0, "高度来自 measure");
    }

    #[test]
    fn measure_records_all_available_space_variants() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        // flex 容器会先用 Min/MaxContent 探固有尺寸，再用确定宽度排布 ⇒ 三种都会到达闭包。
        let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
        let record = seen.clone();
        let row = {
            let mut style = fixed(200.0, 100.0);
            style.inner.align_items = taffy::style::AlignItems::FLEX_START;
            add(&mut tree, root, style)
        };
        let _node = NodeBuilder::boxed()
            .measure(Box::new(move |_known, available| {
                record.borrow_mut().push(available.width);
                Size::new(80.0, 10.0)
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
    fn nan_width_is_invalid_measure_x() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .measure(Box::new(|_known, _available| Size::new(f32::NAN, 10.0)))
            .build(&mut tree, root)
            .expect("挂载成功");

        let error = layout(&mut tree).expect_err("NaN 必须报错");

        assert!(matches!(
            error,
            EngineError::InvalidMeasure {
                node: reported,
                axis: Axis::X,
                value,
            } if reported == node && value.is_nan()
        ));
    }

    #[test]
    fn negative_height_is_invalid_measure_y() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .measure(Box::new(|_known, _available| Size::new(10.0, -1.0)))
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
                assert_eq!(value, -1.0);
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
                .measure(Box::new(|_known, _available| Size::new(f32::NAN, 1.0)))
                .build(&mut tree, root)
                .expect("挂载成功");
            let _second = NodeBuilder::boxed()
                .measure(Box::new(|_known, _available| Size::new(1.0, f32::INFINITY)))
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
            .measure(Box::new(|_known, _available| Size::new(f32::NAN, f32::NAN)))
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
                Size::new(40.0, 10.0)
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
}

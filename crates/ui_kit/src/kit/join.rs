#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JoinedEdges {
    pub corner_radius: [f32; 4],
    pub border_widths: [f32; 4],
}

#[must_use]
pub fn joined_edges(radius: f32, border_width: f32, index: usize, count: usize) -> JoinedEdges {
    debug_assert!(
        count == 0 || index < count,
        "joined index {index} out of {count} children"
    );
    if count == 0 {
        return JoinedEdges {
            corner_radius: [0.0; 4],
            border_widths: [0.0; 4],
        };
    }
    let first = index == 0;
    let last = index + 1 >= count;
    JoinedEdges {
        corner_radius: [
            if first { radius } else { 0.0 },
            if last { radius } else { 0.0 },
            if last { radius } else { 0.0 },
            if first { radius } else { 0.0 },
        ],
        border_widths: [
            border_width,
            border_width,
            border_width,
            if first { border_width } else { 0.0 },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::joined_edges;

    #[test]
    fn a_single_child_keeps_every_corner_and_border() {
        let edges = joined_edges(6.0, 1.0, 0, 1);
        assert_eq!(edges.corner_radius, [6.0; 4]);
        assert_eq!(edges.border_widths, [1.0; 4]);
    }

    #[test]
    fn a_row_of_three_only_rounds_the_outer_corners() {
        let first = joined_edges(6.0, 1.0, 0, 3);
        assert_eq!(first.corner_radius, [6.0, 0.0, 0.0, 6.0]);
        assert_eq!(first.border_widths, [1.0, 1.0, 1.0, 1.0]);

        let middle = joined_edges(6.0, 1.0, 1, 3);
        assert_eq!(middle.corner_radius, [0.0; 4]);
        assert_eq!(middle.border_widths, [1.0, 1.0, 1.0, 0.0]);

        let last = joined_edges(6.0, 1.0, 2, 3);
        assert_eq!(last.corner_radius, [0.0, 6.0, 6.0, 0.0]);
        assert_eq!(last.border_widths, [1.0, 1.0, 1.0, 0.0]);
    }

    #[test]
    fn shared_edges_keep_exactly_one_border() {
        let count = 4;
        let vertical_edges: f32 = (0..count)
            .map(|index| {
                let edges = joined_edges(4.0, 1.0, index, count);
                edges.border_widths[1] + edges.border_widths[3]
            })
            .sum();
        assert_eq!(
            vertical_edges,
            (count + 1) as f32,
            "4 个元素应有 5 条竖边（每条宽度 1）"
        );
    }

    #[test]
    fn empty_rows_are_all_zero() {
        let edges = joined_edges(6.0, 1.0, 0, 0);
        assert_eq!(edges.corner_radius, [0.0; 4]);
        assert_eq!(edges.border_widths, [0.0; 4]);
    }

    #[test]
    fn the_legacy_left_right_rule_is_preserved() {
        for count in 1..=5 {
            for index in 0..count {
                let edges = joined_edges(8.0, 2.0, index, count);
                let is_first = index == 0;
                let is_last = index + 1 == count;
                assert_eq!(edges.corner_radius[0], if is_first { 8.0 } else { 0.0 });
                assert_eq!(edges.corner_radius[3], if is_first { 8.0 } else { 0.0 });
                assert_eq!(edges.corner_radius[1], if is_last { 8.0 } else { 0.0 });
                assert_eq!(edges.corner_radius[2], if is_last { 8.0 } else { 0.0 });
                assert_eq!(edges.border_widths[3], if is_first { 2.0 } else { 0.0 });
            }
        }
    }
}

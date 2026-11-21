//! 拼接控件（Button / Select 横向连成一组）的共享边角规则。
//!
//! # 旧实现的语义（保持不变）
//!
//! 旧 `crate::join::Join` 把若干元素横排，并对非首元素 `rounded_l_none().border_l_0()`、对非尾
//! 元素 `rounded_r_none()`：相邻元素之间只留**一条**边框（左侧元素的右边框），两端保留外圆角。
//! 这是唯一需要跨元素协调的样式，本模块把它抽成**纯函数**（输入：圆角半径、边框宽度、元素下标
//! 与总数；输出：该元素应使用的圆角与四边边框宽度）。
//!
//! # 为什么不是 `Join` 元素
//!
//! 引擎的组件是**挂载期 builder**（`NodeBuilder` 一次性消费，`build` 后不可再改样式），没有
//! 旧 GPUI 那种「先建元素、再在父元素里 `when` 调整子元素」的模型。因此「拼接」在新架构里只能
//! 发生在**构造每个子元素之前**：Button / Select 的 builder（P2-4）拿 `index/count` 调本函数，
//! 把返回值写进自己的 `Style` 与 `PaintSpec`，再按顺序挂到同一个 flex 行容器里。
//! 旧 `Join::button/select` 的元素组合部分随 Button/Select 一起在 P2-4 落地，本批只交付规则
//! （登记见 spec-delta §6）。
//!
//! # 已知差异（登记见 spec-delta §3）
//!
//! 引擎把「布局边框」（`Style.inner.border`，占位）与「绘制边框」（`PaintSpec.border`，单边宽
//! 统一的一条内描边）拆成了两处。旧 `.border_l_0()` 同时消掉二者；新实现能精确表达**布局**边框
//! 的四边差异（[`JoinedEdges::border_widths`]），但 `PaintSpec.border` 只有统一宽度 —— 绘制侧
//! 的「共享边只画一条线」需要 P2-4 在 `PaintBuilder` 之上按 `border_widths` 决定画哪几条线
//! （或整组只画外框）。

/// 一个拼接元素应使用的边角与边框（CSS 顺序）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JoinedEdges {
    /// 四角半径 `[tl, tr, br, bl]`（逻辑像素）。
    pub corner_radius: [f32; 4],
    /// 四边**布局**边框宽度 `[top, right, bottom, left]`（逻辑像素）。
    pub border_widths: [f32; 4],
}

/// 拼接行里第 `index` 个元素（共 `count` 个）应使用的边角与边框。
///
/// 规则（与旧 `Join` 等价）：
/// - 首元素保留左侧圆角与左边框；非首元素左侧圆角 = 0、左边框 = 0（与左邻共享边）；
/// - 尾元素保留右侧圆角；非尾元素右侧圆角 = 0（与右邻共享边）；
/// - 每个元素保留自己的右边框 ⇒ 相邻两元素之间恰好一条边（左邻的右边框）。
///
/// `count == 0` 是空拼接（返回全 0，调用方不应渲染任何子元素）。`index >= count` **不是合法
/// 输入**：debug 构建由 `debug_assert!` 直接抓调用错误；release 构建不 panic，但结果是「按
/// 尾元素算」（`first = false` / `last = true`，即右圆角保留、左圆角归零）—— 这只是「不 panic
/// 的产物」，**不是**契约，不要依赖它（审查 F9 指出的文档与 release 行为不符已按此更正）。
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

    /// 单元素：四角与四边都保留（与不拼接时一致）。
    #[test]
    fn a_single_child_keeps_every_corner_and_border() {
        let edges = joined_edges(6.0, 1.0, 0, 1);
        assert_eq!(edges.corner_radius, [6.0; 4]);
        assert_eq!(edges.border_widths, [1.0; 4]);
    }

    /// 三元素：首元素左圆角 + 左边框；中间只有右邻共享边；尾元素右圆角。
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

    /// 共享边只画一条（左邻的右边框）：整个拼接行的竖直边线总数 = 元素数 + 1（含外框）。
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

    /// 空拼接返回全 0（调用方不应渲染）；越界下标 debug 下断言失败。
    #[test]
    fn empty_rows_are_all_zero() {
        let edges = joined_edges(6.0, 1.0, 0, 0);
        assert_eq!(edges.corner_radius, [0.0; 4]);
        assert_eq!(edges.border_widths, [0.0; 4]);
    }

    /// 旧 `square_shared_corners` 的等价性：非首元素清左侧、非尾元素清右侧（与 `JoinedEdges`
    /// 的 CSS 顺序一一对应）。
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

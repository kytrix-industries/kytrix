//! macOS 自绘标题栏**拖动区**的命中判定与指针事件配对纯逻辑（X2）。
//!
//! # 坐标语义
//!
//! 拖动区是**逻辑窗口坐标**：客户区左上角为原点、x 向右、y 向下（与
//! `Window::set_drag_regions` 文档一致）。`GuiView` 实现了 `isFlipped == YES`，因此
//! `-[NSView convertPoint:fromView:nil]` 返回的 view 坐标本来就是左上原点、y 向下 ——
//! 平台层用 `mouseDown:` 的 `event.locationInWindow` 经同一转换得到引擎逻辑点，**不需要**
//! 再做 AppKit 左下原点的翻转。这里因此只做矩形包含判定。
//!
//! # 命中之后的指针事件配对（RB-08）
//!
//! 命中拖动区的 `mouseDown:` 把拖动交给 WindowServer（`performWindowDragWithEvent:`），
//! **不**投递 `PointerDown`。Apple 文档对 `performWindowDragWithEvent:` 明确「可能不再发
//! mouseUp」，但 AppKit 仍可能补发主键的 `mouseUp:` / `mouseDragged:` —— 它们没有配对的
//! Down，投给应用就违反 `Window::set_drag_regions` 的「拖动期间不再投递指针事件」契约。
//! [`WindowDragState`] 记录「当前这一次主键交互是否被拖动区消费」，由它决定后续主键
//! Up / Dragged 是吞掉还是投递。
//!
//! # 为什么在 `platform/` 根而不是 `platform/macos/` 下
//!
//! `platform::macos` 整模块只在 macOS 目标编译（objc2 依赖也是 macOS-only）；命中判定与配对
//! 状态机是纯数据 + 纯函数，必须在开发机（Windows）单测 —— 与
//! [`crate::platform::macos_placement`]、[`crate::platform::macos_close`]、
//! [`crate::platform::macos_owner`] 同一模式。真机侧只负责把 `mouseDown:` 的逻辑点喂进来、
//! 命中时调 `performWindowDragWithEvent:`，并按状态机结论决定是否 `push_pointer`。

use crate::math::{Point, Rect};

/// 命中拖动区后的**主键指针事件配对**状态（X2/RB-08）。
///
/// 只有「命中拖动区且真的发起了原生拖动」才进入拖动状态（`GuiView::begin_window_drag`
/// 在窗口已消失时返回 `false`，那种情况仍要投递 `PointerDown`，否则这次点击被静默吃掉）。
/// 状态在三个位置清除，因此**不会永久吞事件**：
///
/// - 主键 `mouseUp:`（拖动结束时 AppKit 若补发 Up，吞掉并清状态）；
/// - 主键 `mouseDown:`（下一次点击的命中判定直接覆盖状态）；
/// - `mouseMoved:`（Apple 约定只在无按键时投递的悬停移动，证明拖动会话已结束，见
///   [`Self::hover_move`]；实现仍用 `NSEvent.pressedMouseButtons` 兜底，主键仍按下时不清）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowDragState {
    /// 主键按下命中了拖动区、已把拖动交给 WindowServer（没有投递 `PointerDown`）。
    dragging: bool,
}

impl WindowDragState {
    /// `mouseDown:` 的处置：返回 `true` = 投递 `PointerDown`。
    ///
    /// `hit` = 本次按下命中了拖动区**且**原生拖动已发起。命中 ⇒ 进入拖动状态、吞掉 Down；
    /// 否则退出拖动状态并正常投递 —— 上一次拖动若没收到 `mouseUp:`，这次点击也会把状态
    /// 清干净（不会跨会话吞事件）。
    pub(crate) fn primary_down(&mut self, hit: bool) -> bool {
        self.dragging = hit;
        !hit
    }

    /// 拖动期间的主键 `mouseDragged:` 是否要吞掉。
    pub(crate) fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// 主键 `mouseUp:` 的处置：返回 `true` = 投递 `PointerUp`。
    ///
    /// 拖动期间到达的 Up 没有配对的 Down ⇒ 吞掉并结束拖动状态；正常指针会话的 Up 原样投递。
    pub(crate) fn primary_up(&mut self) -> bool {
        !std::mem::take(&mut self.dragging)
    }

    /// `mouseMoved:`（悬停移动）到达：结束拖动状态（主键仍按下时不动，见下）。
    ///
    /// Apple 文档对 `NSWindow.acceptsMouseMovedEvents` / `mouseMoved:` 的约定是「**没有**鼠标
    /// 按键按下时才投递」（拖动中发 `mouseDragged:`），因此它出现即代表拖动会话已经结束
    /// （即使始终没收到 `mouseUp:`）；不结束的话拖动后的悬停/光标形状会一直冻到下一次点击。
    ///
    /// `primary_button_down` 是调用方读到的 `NSEvent.pressedMouseButtons` 主键位：作为**兜底**
    /// —— 若某个 AppKit 版本在拖动期间仍补发 `mouseMoved:`，主键仍按下 ⇒ 不清状态，后续
    /// 主键 Up/Move 继续按拖动会话吞掉（不会重新泄漏无 Down 配对的事件）。
    pub(crate) fn hover_move(&mut self, primary_button_down: bool) {
        if !primary_button_down {
            self.dragging = false;
        }
    }
}

/// 命中哪个拖动区（按传入顺序返回**第一个**命中的下标；无命中 = `None`）。
///
/// 判定用半开区间 `[x, x + width) × [y, y + height)`：右/下边界不属于区域（相邻区域可以
/// 无缝拼接而不重叠）。非正尺寸（空/负宽高）的区域按空集处理，永不命中；非有限坐标不命中
/// （避免 NaN 比较的未定义语义把点击误判成拖动）。
pub(crate) fn drag_region_at(regions: &[Rect], point: Point) -> Option<usize> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return None;
    }
    regions.iter().position(|region| {
        let (x, y) = (region.origin.x, region.origin.y);
        let (width, height) = (region.size.width, region.size.height);
        width > 0.0
            && height > 0.0
            && point.x >= x
            && point.x < x + width
            && point.y >= y
            && point.y < y + height
    })
}

#[cfg(test)]
mod tests {
    use super::{WindowDragState, drag_region_at};
    use crate::math::{Point, Rect};

    fn point(x: f32, y: f32) -> Point {
        Point::new(x, y)
    }

    /// 没有拖动区（应用的默认状态）⇒ 一次点击都不算拖动区。
    #[test]
    fn no_regions_never_hit() {
        assert_eq!(drag_region_at(&[], point(10.0, 10.0)), None);
    }

    /// 区域内部命中；区域外不命中。
    #[test]
    fn points_inside_and_outside_a_region() {
        let regions = [Rect::new(10.0, 20.0, 100.0, 40.0)];
        assert_eq!(drag_region_at(&regions, point(50.0, 30.0)), Some(0));
        assert_eq!(drag_region_at(&regions, point(9.99, 30.0)), None);
        assert_eq!(drag_region_at(&regions, point(50.0, 19.99)), None);
    }

    /// 边界语义：左上含、右下不含（半开区间）；右/下边界外一像素属于窗口内容。
    #[test]
    fn region_bounds_are_half_open() {
        let regions = [Rect::new(10.0, 20.0, 100.0, 40.0)];
        assert_eq!(
            drag_region_at(&regions, point(10.0, 20.0)),
            Some(0),
            "左上角含"
        );
        assert_eq!(
            drag_region_at(&regions, point(109.9, 59.9)),
            Some(0),
            "右下角内侧命中"
        );
        assert_eq!(
            drag_region_at(&regions, point(110.0, 30.0)),
            None,
            "右边界不含"
        );
        assert_eq!(
            drag_region_at(&regions, point(50.0, 60.0)),
            None,
            "下边界不含"
        );
    }

    /// y 向下语义（自绘顶部条带）：窗口顶部 y=0..38 的条带命中，窗口下方的同样 x 不命中。
    /// `GuiView` 是 flipped，`logical_point` 返回的正是这套坐标（无需再翻转）。
    #[test]
    fn regions_use_top_left_origin_with_y_pointing_down() {
        let regions = [Rect::new(0.0, 0.0, 400.0, 38.0)];
        assert_eq!(
            drag_region_at(&regions, point(200.0, 10.0)),
            Some(0),
            "顶部条带"
        );
        assert_eq!(
            drag_region_at(&regions, point(200.0, 500.0)),
            None,
            "窗口底部（y 向下更大）不在条带内"
        );
    }

    /// 多个区域：返回第一个命中的下标；相邻区域无缝拼接不重叠。
    #[test]
    fn multiple_regions_report_the_first_match() {
        let regions = [
            Rect::new(0.0, 0.0, 100.0, 30.0),
            Rect::new(100.0, 0.0, 100.0, 30.0),
            Rect::new(0.0, 0.0, 10.0, 10.0),
        ];
        assert_eq!(drag_region_at(&regions, point(5.0, 5.0)), Some(0));
        assert_eq!(drag_region_at(&regions, point(150.0, 5.0)), Some(1));
        assert_eq!(
            drag_region_at(&regions, point(100.0, 5.0)),
            Some(1),
            "拼接处归后一个区域"
        );
        assert_eq!(drag_region_at(&regions, point(250.0, 5.0)), None);
    }

    /// 非正尺寸（空/负宽高）的区域是空集，不命中；非有限点也不命中。
    #[test]
    fn degenerate_regions_and_points_never_hit() {
        let regions = [
            Rect::new(0.0, 0.0, 0.0, 30.0),
            Rect::new(0.0, 0.0, 100.0, 0.0),
            Rect::new(0.0, 0.0, -10.0, -10.0),
        ];
        assert_eq!(drag_region_at(&regions, point(0.0, 0.0)), None);
        let finite = [Rect::new(0.0, 0.0, 100.0, 30.0)];
        assert_eq!(drag_region_at(&finite, point(f32::NAN, 5.0)), None);
        assert_eq!(drag_region_at(&finite, point(5.0, f32::INFINITY)), None);
    }

    // ── RB-08：命中拖动区后的主键 Up / Move 配对 ──────────────────────────────

    /// RB-08 核心回归：命中拖动区的那次按下没有投递 `PointerDown`，随后 AppKit 补发的
    /// 主键 `mouseUp:` 必须被吞掉（否则应用收到无 Down 配对的 Up）。
    #[test]
    fn a_drag_region_hit_swallows_the_following_mouse_up() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true), "命中拖动区不得投递 PointerDown");
        assert!(state.is_dragging());
        assert!(!state.primary_up(), "拖动区的 Up 没有配对 Down，必须吞掉");
        assert!(!state.is_dragging(), "拖动状态在 Up 上结束");
        // 下一次正常点击的 Up 必须投递（状态没有粘住）。
        assert!(state.primary_down(false));
        assert!(state.primary_up());
    }

    /// 拖动期间的主键 `mouseDragged:` 同样没有配对的 Down：一律吞掉。
    #[test]
    fn a_drag_region_hit_swallows_the_dragged_moves() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true));
        assert!(state.is_dragging(), "拖动区命中期间不得投递主键 Move");
        assert!(!state.primary_up());
        assert!(!state.is_dragging(), "拖动结束后 Move 恢复投递");
    }

    /// 拖动区外的一次点击是正常指针会话：Down / Up 都投递，配对完整。
    #[test]
    fn a_click_outside_the_drag_region_keeps_the_pointer_session_paired() {
        let mut state = WindowDragState::default();
        assert!(state.primary_down(false), "拖动区外必须投递 PointerDown");
        assert!(!state.is_dragging());
        assert!(state.primary_up(), "正常会话的 PointerUp 必须投递");
    }

    /// 命中拖动区但原生拖动没能发起（窗口已消失）：点击不得被静默吃掉，仍要投递 Down。
    #[test]
    fn a_drag_region_hit_without_a_native_drag_still_delivers_the_down() {
        let mut state = WindowDragState::default();
        assert!(
            state.primary_down(false),
            "begin_window_drag 返回 false 时必须回退成普通点击"
        );
        assert!(state.primary_up());
    }

    /// 拖动后 AppKit 可能始终不发 `mouseUp:`：`mouseMoved:`（无按键）出现即证明拖动会话
    /// 已结束，状态必须在这里清掉 —— 否则拖动结束后悬停会冻到下一次点击（永久吞 Move）。
    #[test]
    fn a_hover_move_ends_a_drag_that_never_reported_mouse_up() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true));
        state.hover_move(false);
        assert!(!state.is_dragging(), "悬停移动证明拖动已结束");
        // 拖动结束后的悬停移动照常投递（调用方无条件 push，这里只校验状态已清）。
        // 之后的一次新点击必须是干净的会话。
        assert!(state.primary_down(false));
        assert!(state.primary_up());
    }

    /// 兜底：若某个 AppKit 版本在拖动期间仍补发 `mouseMoved:`（主键仍按下），**不得**提前
    /// 清拖动状态 —— 否则后续补发的 Up/Move 会重新变成无 Down 配对的事件（RB-08 回归）。
    #[test]
    fn a_hover_move_while_the_primary_button_is_still_down_keeps_the_drag_state() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true));
        state.hover_move(true);
        assert!(state.is_dragging(), "主键仍按下时不得清拖动状态");
        assert!(
            !state.primary_up(),
            "拖动中的 Up 仍必须吞掉（不能因为一次 mouseMoved 就泄漏）"
        );
        assert!(!state.is_dragging());
    }

    /// 上一次拖动没收到 `mouseUp:` 时，下一次**非命中**的 `mouseDown:` 也会清状态：
    /// 新会话的 Down / Up 都投递，不会被旧状态吞掉。
    #[test]
    fn a_second_click_after_an_unfinished_drag_starts_a_clean_session() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true));
        // 没有 Up、没有 hover：直接下一次点击。
        assert!(state.primary_down(false), "新会话的 Down 必须投递");
        assert!(state.primary_up(), "新会话的 Up 必须投递");
    }
}

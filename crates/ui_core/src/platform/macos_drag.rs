use crate::math::{Point, Rect};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowDragState {
    dragging: bool,
}

impl WindowDragState {
    pub(crate) fn primary_down(&mut self, hit: bool) -> bool {
        self.dragging = hit;
        !hit
    }

    pub(crate) fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub(crate) fn primary_up(&mut self) -> bool {
        !std::mem::take(&mut self.dragging)
    }

    pub(crate) fn hover_move(&mut self, primary_button_down: bool) -> bool {
        if primary_button_down {
            return false;
        }
        self.dragging = false;
        true
    }
}

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

    #[test]
    fn no_regions_never_hit() {
        assert_eq!(drag_region_at(&[], point(10.0, 10.0)), None);
    }

    #[test]
    fn points_inside_and_outside_a_region() {
        let regions = [Rect::new(10.0, 20.0, 100.0, 40.0)];
        assert_eq!(drag_region_at(&regions, point(50.0, 30.0)), Some(0));
        assert_eq!(drag_region_at(&regions, point(9.99, 30.0)), None);
        assert_eq!(drag_region_at(&regions, point(50.0, 19.99)), None);
    }

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

    #[test]
    fn a_drag_region_hit_swallows_the_following_mouse_up() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true), "命中拖动区不得投递 PointerDown");
        assert!(state.is_dragging());
        assert!(!state.primary_up(), "拖动区的 Up 没有配对 Down，必须吞掉");
        assert!(!state.is_dragging(), "拖动状态在 Up 上结束");
        assert!(state.primary_down(false));
        assert!(state.primary_up());
    }

    #[test]
    fn a_drag_region_hit_swallows_the_dragged_moves() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true));
        assert!(state.is_dragging(), "拖动区命中期间不得投递主键 Move");
        assert!(!state.primary_up());
        assert!(!state.is_dragging(), "拖动结束后 Move 恢复投递");
    }

    #[test]
    fn a_click_outside_the_drag_region_keeps_the_pointer_session_paired() {
        let mut state = WindowDragState::default();
        assert!(state.primary_down(false), "拖动区外必须投递 PointerDown");
        assert!(!state.is_dragging());
        assert!(state.primary_up(), "正常会话的 PointerUp 必须投递");
    }

    #[test]
    fn a_drag_region_hit_without_a_native_drag_still_delivers_the_down() {
        let mut state = WindowDragState::default();
        assert!(
            state.primary_down(false),
            "begin_window_drag 返回 false 时必须回退成普通点击"
        );
        assert!(state.primary_up());
    }

    #[test]
    fn a_hover_move_ends_a_drag_that_never_reported_mouse_up() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true));
        assert!(
            state.hover_move(false),
            "主键未按下：结束拖动并投递这条 Move"
        );
        assert!(!state.is_dragging(), "悬停移动证明拖动已结束");
        assert!(state.primary_down(false));
        assert!(state.primary_up());
    }

    #[test]
    fn a_hover_move_while_the_primary_button_is_still_down_keeps_the_drag_state() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true));
        assert!(
            !state.hover_move(true),
            "主键仍按下：这条 Move 不得投递（只保护状态不保护投递仍会泄漏）"
        );
        assert!(state.is_dragging(), "主键仍按下时不得清拖动状态");
        assert!(
            !state.primary_up(),
            "拖动中的 Up 仍必须吞掉（不能因为一次 mouseMoved 就泄漏）"
        );
        assert!(!state.is_dragging());
    }

    #[test]
    fn a_second_click_after_an_unfinished_drag_starts_a_clean_session() {
        let mut state = WindowDragState::default();
        assert!(!state.primary_down(true));
        assert!(state.primary_down(false), "新会话的 Down 必须投递");
        assert!(state.primary_up(), "新会话的 Up 必须投递");
    }
}

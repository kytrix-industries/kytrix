use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    math::{LogicalPoint, LogicalRect, px},
    popup::Side,
};

pub(crate) const SUBMENU_OPEN_DELAY: Duration = Duration::from_millis(200);
pub(crate) const POINTER_CLOSE_GRACE: Duration = Duration::from_millis(300);
pub(crate) const TYPEAHEAD_TIMEOUT: Duration = Duration::from_millis(2000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoverTimings {
    pub open_delay: Duration,
    pub close_grace: Duration,
    pub typeahead_timeout: Duration,
}

impl Default for HoverTimings {
    fn default() -> Self {
        Self {
            open_delay: SUBMENU_OPEN_DELAY,
            close_grace: POINTER_CLOSE_GRACE,
            typeahead_timeout: TYPEAHEAD_TIMEOUT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct LevelId(usize);

impl LevelId {
    pub(crate) const fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LevelState {
    Closed,
    Opening,
    Open,
    Closing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenSource {
    Pointer,
    Keyboard,
    Click,
}

impl OpenSource {
    pub(crate) const fn is_pointer(self) -> bool {
        matches!(self, Self::Pointer)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SafeTriangle {
    points: [LogicalPoint; 5],
}

impl SafeTriangle {
    fn new(exit: LogicalPoint, submenu: LogicalRect, side: Side) -> Option<Self> {
        if !exit.x.is_finite() || !exit.y.is_finite() || !submenu.is_finite() {
            return None;
        }
        let bleed = if side == Side::Right { -5.0 } else { 5.0 };
        let near = if side == Side::Right {
            submenu.origin.x
        } else {
            submenu.max().x
        };
        let far = if side == Side::Right {
            submenu.max().x
        } else {
            submenu.origin.x
        };
        Some(Self {
            points: [
                LogicalPoint::new(px(exit.x.value() + bleed), exit.y),
                LogicalPoint::new(near, submenu.origin.y),
                LogicalPoint::new(far, submenu.origin.y),
                LogicalPoint::new(far, submenu.max().y),
                LogicalPoint::new(near, submenu.max().y),
            ],
        })
    }

    pub(crate) fn contains(self, point: LogicalPoint) -> bool {
        point_in_polygon(&self.points, point)
    }

    #[cfg(test)]
    pub(crate) fn points(self) -> [LogicalPoint; 5] {
        self.points
    }
}

fn point_in_polygon(points: &[LogicalPoint], point: LogicalPoint) -> bool {
    if !point.x.is_finite() || !point.y.is_finite() {
        return false;
    }
    let x = point.x.value();
    let y = point.y.value();
    let mut inside = false;
    let mut previous = points.len() - 1;
    for index in 0..points.len() {
        let xi = points[index].x.value();
        let yi = points[index].y.value();
        let xj = points[previous].x.value();
        let yj = points[previous].y.value();
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        previous = index;
    }
    inside
}

fn derived_safe_triangle(anchor: LogicalRect, submenu: LogicalRect) -> Option<SafeTriangle> {
    let side = side_of(anchor, submenu);
    let edge = if side == Side::Right {
        anchor.max().x
    } else {
        anchor.origin.x
    };
    let exit = LogicalPoint::new(
        edge,
        px((anchor.origin.y.value() + anchor.max().y.value()) / 2.0),
    );
    SafeTriangle::new(exit, submenu, side)
}

fn side_of(anchor: LogicalRect, submenu: LogicalRect) -> Side {
    if submenu.origin.x >= anchor.max().x - 1.0 {
        Side::Right
    } else if submenu.max().x <= anchor.origin.x + 1.0 {
        Side::Left
    } else {
        Side::Right
    }
}

#[derive(Clone, Copy, Debug)]
struct LevelSlot {
    parent: Option<LevelId>,
    row: usize,
    state: LevelState,
    source: OpenSource,
    rect: Option<LogicalRect>,
    anchor: Option<LogicalRect>,
    exit: Option<LogicalPoint>,
    grace: Option<SafeTriangle>,
    active: Option<usize>,
    child: Option<LevelId>,
}

impl LevelSlot {
    fn new(parent: Option<LevelId>, row: usize, state: LevelState, source: OpenSource) -> Self {
        Self {
            parent,
            row,
            state,
            source,
            rect: None,
            anchor: None,
            exit: None,
            grace: None,
            active: None,
            child: None,
        }
    }

    fn clear_relations(&mut self) {
        self.state = LevelState::Closed;
        self.grace = None;
        self.exit = None;
        self.active = None;
        self.child = None;
    }
}

#[derive(Clone, Debug, Default)]
struct Typeahead {
    level: Option<LevelId>,
    buffer: SmolStr,
}

struct ChainInner {
    levels: RefCell<Vec<LevelSlot>>,
    timings: HoverTimings,
    typeahead: RefCell<Typeahead>,
    pointer_entered: Cell<bool>,
}

#[derive(Clone)]
pub(crate) struct MenuChain {
    inner: Rc<ChainInner>,
}

impl MenuChain {
    pub(crate) fn new(timings: HoverTimings) -> Self {
        Self {
            inner: Rc::new(ChainInner {
                levels: RefCell::new(Vec::new()),
                timings,
                typeahead: RefCell::new(Typeahead::default()),
                pointer_entered: Cell::new(false),
            }),
        }
    }

    pub(crate) fn timings(&self) -> HoverTimings {
        self.inner.timings
    }

    pub(crate) fn reset(&self) {
        self.inner.levels.borrow_mut().clear();
        let mut typeahead = self.inner.typeahead.borrow_mut();
        typeahead.level = None;
        typeahead.buffer = SmolStr::default();
        drop(typeahead);
        self.inner.pointer_entered.set(false);
    }

    pub(crate) fn open_root(&self, rect: Option<LogicalRect>, source: OpenSource) -> LevelId {
        let mut levels = self.inner.levels.borrow_mut();
        levels.clear();
        let mut root = LevelSlot::new(None, 0, LevelState::Open, source);
        root.rect = rect;
        levels.push(root);
        drop(levels);
        self.inner.pointer_entered.set(false);
        LevelId(0)
    }

    pub(crate) fn begin_open(&self, parent: LevelId, row: usize, source: OpenSource) -> LevelId {
        let mut levels = self.inner.levels.borrow_mut();
        let reused = levels.iter().position(|slot| {
            slot.state == LevelState::Closed && slot.parent == Some(parent) && slot.row == row
        });
        let id = match reused {
            Some(index) => {
                levels[index] = LevelSlot::new(Some(parent), row, LevelState::Opening, source);
                LevelId(index)
            }
            None => {
                levels.push(LevelSlot::new(
                    Some(parent),
                    row,
                    LevelState::Opening,
                    source,
                ));
                LevelId(levels.len() - 1)
            }
        };
        if let Some(parent_slot) = levels.get_mut(parent.index())
            && parent_slot.state != LevelState::Closed
        {
            parent_slot.child = Some(id);
        }
        id
    }

    pub(crate) fn confirm_open(
        &self,
        level: LevelId,
        rect: Option<LogicalRect>,
        source: OpenSource,
    ) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return false;
        };
        if !matches!(slot.state, LevelState::Opening | LevelState::Open) {
            return false;
        }
        let changed =
            rect.is_some_and(|rect| slot.rect != Some(rect)) || slot.state != LevelState::Open;
        if rect.is_some() {
            slot.rect = rect;
        }
        slot.source = source;
        slot.state = LevelState::Open;
        changed
    }

    pub(crate) fn cancel_open(&self, level: LevelId) {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return;
        };
        if slot.state != LevelState::Opening {
            return;
        }
        slot.clear_relations();
        if let Some(parent) = slot.parent
            && let Some(parent_slot) = levels.get_mut(parent.index())
            && parent_slot.child == Some(level)
        {
            parent_slot.child = None;
        }
    }

    pub(crate) fn close(&self, level: LevelId) {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return;
        };
        let parent = slot.parent;
        slot.clear_relations();
        if let Some(parent) = parent
            && let Some(parent_slot) = levels.get_mut(parent.index())
            && parent_slot.child == Some(level)
        {
            parent_slot.child = None;
        }
        let descendants: Vec<usize> = (0..levels.len())
            .filter(|index| Levels::is_descendant(&levels, *index, level))
            .collect();
        for index in descendants {
            if let Some(slot) = levels.get_mut(index) {
                slot.clear_relations();
            }
        }
    }

    pub(crate) fn state(&self, level: LevelId) -> Option<LevelState> {
        self.inner
            .levels
            .borrow()
            .get(level.index())
            .map(|slot| slot.state)
    }

    #[cfg(test)]
    pub(crate) fn source(&self, level: LevelId) -> Option<OpenSource> {
        self.inner
            .levels
            .borrow()
            .get(level.index())
            .map(|slot| slot.source)
    }

    pub(crate) fn row(&self, level: LevelId) -> Option<usize> {
        self.inner
            .levels
            .borrow()
            .get(level.index())
            .map(|slot| slot.row)
    }

    pub(crate) fn parent(&self, level: LevelId) -> Option<LevelId> {
        self.inner
            .levels
            .borrow()
            .get(level.index())
            .and_then(|slot| slot.parent)
    }

    pub(crate) fn child(&self, level: LevelId) -> Option<LevelId> {
        let levels = self.inner.levels.borrow();
        let child = levels.get(level.index()).and_then(|slot| slot.child)?;
        levels
            .get(child.index())
            .filter(|slot| slot.state != LevelState::Closed)
            .map(|_| child)
    }

    pub(crate) fn rect(&self, level: LevelId) -> Option<LogicalRect> {
        self.inner
            .levels
            .borrow()
            .get(level.index())
            .and_then(|slot| slot.rect)
    }

    pub(crate) fn set_rect(&self, level: LevelId, rect: LogicalRect) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return false;
        };
        if slot.rect == Some(rect) {
            return false;
        }
        slot.rect = Some(rect);
        true
    }

    #[cfg(test)]
    pub(crate) fn anchor(&self, level: LevelId) -> Option<LogicalRect> {
        self.inner
            .levels
            .borrow()
            .get(level.index())
            .and_then(|slot| slot.anchor)
    }

    pub(crate) fn set_anchor(&self, level: LevelId, anchor: LogicalRect) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return false;
        };
        if slot.anchor == Some(anchor) {
            return false;
        }
        slot.anchor = Some(anchor);
        true
    }

    pub(crate) fn depth(&self, level: LevelId) -> usize {
        let levels = self.inner.levels.borrow();
        Levels::depth(&levels, level)
    }

    pub(crate) fn open_levels(&self) -> Vec<LevelId> {
        let levels = self.inner.levels.borrow();
        let mut open: Vec<(usize, LevelId)> = levels
            .iter()
            .enumerate()
            .filter(|(_, slot)| matches!(slot.state, LevelState::Open | LevelState::Closing))
            .map(|(index, _)| (Levels::depth(&levels, LevelId(index)), LevelId(index)))
            .collect();
        open.sort_unstable();
        open.into_iter().map(|(_, level)| level).collect()
    }

    #[cfg(test)]
    pub(crate) fn deepest_open(&self) -> Option<LevelId> {
        self.open_levels().into_iter().last()
    }

    pub(crate) fn deepest_kept(&self) -> Option<LevelId> {
        let levels = self.inner.levels.borrow();
        let mut kept: Option<(usize, LevelId)> = None;
        for (index, slot) in levels.iter().enumerate() {
            if !matches!(slot.state, LevelState::Open | LevelState::Closing)
                || slot.source.is_pointer()
            {
                continue;
            }
            let candidate = (Levels::depth(&levels, LevelId(index)), LevelId(index));
            if kept.is_none_or(|current| candidate.0 >= current.0) {
                kept = Some(candidate);
            }
        }
        kept.map(|(_, level)| level)
    }

    pub(crate) fn mark_closing(&self, level: LevelId) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return false;
        };
        if slot.state != LevelState::Open {
            return false;
        }
        slot.state = LevelState::Closing;
        true
    }

    pub(crate) fn reopen(&self, level: LevelId) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return false;
        };
        if slot.state != LevelState::Closing {
            return false;
        }
        slot.state = LevelState::Open;
        true
    }

    pub(crate) fn revive(&self, level: LevelId, rect: Option<LogicalRect>) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return false;
        };
        if slot.state != LevelState::Closed {
            return false;
        }
        slot.state = LevelState::Open;
        slot.grace = None;
        slot.exit = None;
        if let Some(rect) = rect {
            slot.rect = Some(rect);
        }
        let parent = slot.parent;
        drop(levels);
        if let Some(parent) = parent {
            let mut levels = self.inner.levels.borrow_mut();
            if let Some(slot) = levels.get_mut(parent.index()) {
                slot.child = Some(level);
            }
        }
        true
    }

    pub(crate) fn closing_levels(&self) -> Vec<LevelId> {
        let levels = self.inner.levels.borrow();
        levels
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.state == LevelState::Closing)
            .map(|(index, _)| LevelId(index))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn active(&self, level: LevelId) -> Option<usize> {
        self.inner
            .levels
            .borrow()
            .get(level.index())
            .and_then(|slot| slot.active)
    }

    pub(crate) fn set_active(&self, level: LevelId, active: Option<usize>) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return false;
        };
        if slot.active == active {
            return false;
        }
        slot.active = active;
        true
    }

    pub(crate) fn set_exit(&self, level: LevelId, exit: LogicalPoint) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return false;
        };
        if slot.exit == Some(exit) {
            return false;
        }
        slot.exit = Some(exit);
        true
    }

    pub(crate) fn compute_grace(&self, level: LevelId) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get(level.index()) else {
            return false;
        };
        let (Some(exit), Some(rect), Some(anchor)) = (slot.exit, slot.rect, slot.anchor) else {
            return false;
        };
        let grace = SafeTriangle::new(exit, rect, side_of(anchor, rect));
        if slot.grace == grace {
            return false;
        }
        if let Some(slot) = levels.get_mut(level.index()) {
            slot.grace = grace;
        }
        true
    }

    pub(crate) fn clear_grace(&self, level: LevelId) -> bool {
        let mut levels = self.inner.levels.borrow_mut();
        let Some(slot) = levels.get_mut(level.index()) else {
            return false;
        };
        slot.grace.take().is_some()
    }

    pub(crate) fn grace(&self, level: LevelId) -> Option<SafeTriangle> {
        self.inner
            .levels
            .borrow()
            .get(level.index())
            .and_then(|slot| slot.grace)
    }

    pub(crate) fn has_grace(&self, level: LevelId) -> bool {
        self.grace(level).is_some()
    }

    pub(crate) fn grace_contains(&self, point: LogicalPoint) -> Option<LevelId> {
        let levels = self.inner.levels.borrow();
        levels.iter().enumerate().find_map(|(index, slot)| {
            if slot.state == LevelState::Closed {
                return None;
            }
            slot.grace
                .filter(|grace| grace.contains(point))
                .map(|_| LevelId(index))
        })
    }

    pub(crate) fn safe_area_contains(&self, point: LogicalPoint) -> bool {
        if self.grace_contains(point).is_some() {
            return true;
        }
        let levels = self.inner.levels.borrow();
        levels.iter().any(|slot| {
            if !matches!(slot.state, LevelState::Open | LevelState::Closing) {
                return false;
            }
            let (Some(anchor), Some(rect)) = (slot.anchor, slot.rect) else {
                return false;
            };
            derived_safe_triangle(anchor, rect).is_some_and(|triangle| triangle.contains(point))
        })
    }

    pub(crate) fn contains(&self, point: LogicalPoint) -> bool {
        let levels = self.inner.levels.borrow();
        levels.iter().any(|slot| {
            if slot.state == LevelState::Closed {
                return false;
            }
            if slot.rect.is_some_and(|rect| rect.contains(point))
                || slot.grace.is_some_and(|grace| grace.contains(point))
            {
                return true;
            }
            let (Some(anchor), Some(rect)) = (slot.anchor, slot.rect) else {
                return false;
            };
            derived_safe_triangle(anchor, rect).is_some_and(|triangle| triangle.contains(point))
        })
    }

    pub(crate) fn note_pointer_entered(&self) {
        self.inner.pointer_entered.set(true);
    }

    pub(crate) fn pointer_entered(&self) -> bool {
        self.inner.pointer_entered.get()
    }

    pub(crate) fn typeahead_push(&self, level: LevelId, character: char) -> SmolStr {
        let mut typeahead = self.inner.typeahead.borrow_mut();
        let buffer = if typeahead.level == Some(level) {
            format_smolstr!("{}{}", typeahead.buffer, character)
        } else {
            format_smolstr!("{character}")
        };
        typeahead.level = Some(level);
        typeahead.buffer = buffer.clone();
        buffer
    }

    pub(crate) fn typeahead_buffer(&self) -> SmolStr {
        self.inner.typeahead.borrow().buffer.clone()
    }

    pub(crate) fn clear_typeahead(&self) {
        let mut typeahead = self.inner.typeahead.borrow_mut();
        typeahead.level = None;
        typeahead.buffer = SmolStr::default();
    }

    pub(crate) fn clear_typeahead_for(&self, level: LevelId) -> bool {
        let mut typeahead = self.inner.typeahead.borrow_mut();
        if typeahead.level != Some(level) {
            return false;
        }
        typeahead.level = None;
        typeahead.buffer = SmolStr::default();
        true
    }
}

struct Levels;

impl Levels {
    fn depth(levels: &[LevelSlot], level: LevelId) -> usize {
        let mut depth = 0;
        let mut cursor = levels.get(level.index()).and_then(|slot| slot.parent);
        while let Some(current) = cursor {
            depth += 1;
            cursor = levels.get(current.index()).and_then(|slot| slot.parent);
        }
        depth
    }

    fn is_descendant(levels: &[LevelSlot], candidate: usize, ancestor: LevelId) -> bool {
        let mut cursor = levels.get(candidate).and_then(|slot| slot.parent);
        while let Some(current) = cursor {
            if current == ancestor {
                return true;
            }
            cursor = levels.get(current.index()).and_then(|slot| slot.parent);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use ui_core::math::{LogicalPoint, LogicalRect, px};

    use super::{
        HoverTimings, LevelState, MenuChain, OpenSource, SafeTriangle, derived_safe_triangle,
    };

    fn rect(x: f32, y: f32, width: f32, height: f32) -> LogicalRect {
        LogicalRect::new(px(x), px(y), px(width), px(height))
    }

    fn point(x: f32, y: f32) -> LogicalPoint {
        LogicalPoint::new(px(x), px(y))
    }

    #[test]
    fn the_grace_triangle_spans_the_exit_point_and_the_submenu_edges() {
        let triangle = SafeTriangle::new(
            point(100.0, 110.0),
            rect(200.0, 100.0, 120.0, 80.0),
            ui_core::popup::Side::Right,
        )
        .expect("triangle");
        let points = triangle.points();
        assert_eq!(points[0], point(95.0, 110.0), "退出点按 -bleed 内缩");
        assert_eq!(points[1], point(200.0, 100.0), "子菜单近端上角");
        assert_eq!(points[2], point(320.0, 100.0), "子菜单远端上角");
        assert_eq!(points[3], point(320.0, 180.0), "子菜单远端下角");
        assert_eq!(points[4], point(200.0, 180.0), "子菜单近端下角");
    }

    #[test]
    fn the_bleed_mirrors_for_a_left_side_submenu() {
        let triangle = SafeTriangle::new(
            point(300.0, 110.0),
            rect(80.0, 100.0, 120.0, 80.0),
            ui_core::popup::Side::Left,
        )
        .expect("triangle");
        let points = triangle.points();
        assert_eq!(points[0], point(305.0, 110.0), "左侧子菜单 bleed 镜像");
        assert_eq!(points[1], point(200.0, 100.0), "近端是子菜单右边缘");
        assert_eq!(points[2], point(80.0, 100.0));
    }

    #[test]
    fn the_triangle_excludes_points_beyond_the_far_edge_and_below_the_base() {
        let triangle = SafeTriangle::new(
            point(100.0, 140.0),
            rect(200.0, 100.0, 120.0, 80.0),
            ui_core::popup::Side::Right,
        )
        .expect("triangle");
        assert!(
            triangle.contains(point(190.0, 150.0)),
            "指向子菜单的斜线内部"
        );
        assert!(triangle.contains(point(250.0, 110.0)), "子菜单区域内部");
        assert!(
            !triangle.contains(point(190.0, 20.0)),
            "三角形上边界之外不算"
        );
        assert!(
            !triangle.contains(point(190.0, 260.0)),
            "三角形下边界之外不算"
        );
        assert!(!triangle.contains(point(60.0, 150.0)), "退出点左侧之外不算");
    }

    #[test]
    fn a_degenerate_triangle_from_non_finite_inputs_is_rejected() {
        assert!(
            SafeTriangle::new(
                point(f32::NAN, 0.0),
                rect(0.0, 0.0, 10.0, 10.0),
                ui_core::popup::Side::Right
            )
            .is_none()
        );
        let triangle = SafeTriangle::new(
            point(0.0, 0.0),
            rect(10.0, 0.0, 10.0, 10.0),
            ui_core::popup::Side::Right,
        )
        .expect("triangle");
        assert!(!triangle.contains(point(f32::NAN, 5.0)));
    }

    #[test]
    fn the_chain_assigns_states_per_level_and_closes_children_first() {
        let chain = MenuChain::new(HoverTimings::default());
        let root = chain.open_root(Some(rect(100.0, 100.0, 200.0, 160.0)), OpenSource::Click);
        let opening = chain.begin_open(root, 1, OpenSource::Pointer);
        assert_eq!(chain.state(opening), Some(LevelState::Opening));
        assert_eq!(chain.child(root), Some(opening));
        let _ = chain.confirm_open(
            opening,
            Some(rect(300.0, 120.0, 180.0, 100.0)),
            OpenSource::Pointer,
        );
        assert_eq!(chain.state(opening), Some(LevelState::Open));
        let deeper = chain.begin_open(opening, 2, OpenSource::Keyboard);
        let _ = chain.confirm_open(
            deeper,
            Some(rect(480.0, 130.0, 160.0, 90.0)),
            OpenSource::Keyboard,
        );
        assert_eq!(chain.deepest_open(), Some(deeper));
        assert_eq!(chain.depth(deeper), 2);
        assert_eq!(
            chain.deepest_kept(),
            Some(deeper),
            "键盘打开的层也是保留边界（非 pointer 层）"
        );

        chain.close(opening);
        assert_eq!(chain.state(opening), Some(LevelState::Closed));
        assert_eq!(chain.state(deeper), Some(LevelState::Closed));
        assert_eq!(chain.child(root), None);
        assert_eq!(chain.deepest_open(), Some(root));
        assert_eq!(chain.deepest_kept(), Some(root));
    }

    #[test]
    fn cancelling_a_pending_open_returns_the_slot_to_closed_and_reusable() {
        let chain = MenuChain::new(HoverTimings::default());
        let root = chain.open_root(None, OpenSource::Click);
        let first = chain.begin_open(root, 2, OpenSource::Pointer);
        chain.cancel_open(first);
        assert_eq!(chain.state(first), Some(LevelState::Closed));
        assert_eq!(chain.child(root), None);
        let again = chain.begin_open(root, 2, OpenSource::Pointer);
        assert_eq!(again, first, "同一父行复用已关闭的层槽");
        assert_eq!(chain.state(again), Some(LevelState::Opening));
    }

    #[test]
    fn closing_and_reopening_a_level_tracks_the_pointer_grace() {
        let chain = MenuChain::new(HoverTimings::default());
        let root = chain.open_root(Some(rect(100.0, 100.0, 200.0, 160.0)), OpenSource::Click);
        let child = chain.begin_open(root, 1, OpenSource::Pointer);
        let _ = chain.confirm_open(
            child,
            Some(rect(300.0, 120.0, 180.0, 100.0)),
            OpenSource::Pointer,
        );
        let _ = chain.set_anchor(child, rect(100.0, 120.0, 200.0, 32.0));
        let _ = chain.set_exit(child, point(295.0, 136.0));
        assert!(chain.compute_grace(child));
        assert!(chain.contains(point(295.0, 136.0)), "退出点属于链");
        assert!(
            chain.contains(point(310.0, 150.0)),
            "子菜单矩形属于链（三角形内）"
        );
        assert!(!chain.contains(point(500.0, 500.0)));
        assert!(chain.mark_closing(child));
        assert_eq!(chain.state(child), Some(LevelState::Closing));
        assert!(chain.reopen(child));
        assert_eq!(chain.state(child), Some(LevelState::Open));
        assert!(chain.clear_grace(child));
        assert!(!chain.has_grace(child));
    }

    #[test]
    fn the_typeahead_buffer_is_per_level_and_resets_on_a_level_change() {
        let chain = MenuChain::new(HoverTimings::default());
        let root = chain.open_root(None, OpenSource::Click);
        let child = chain.begin_open(root, 1, OpenSource::Pointer);
        assert_eq!(chain.typeahead_push(root, 'd'), "d");
        assert_eq!(chain.typeahead_push(root, 'e'), "de");
        assert_eq!(chain.typeahead_push(child, 'x'), "x", "换层清空");
        assert_eq!(chain.typeahead_push(root, 'd'), "d", "回到旧层也重新开始");
        assert_eq!(chain.typeahead_buffer(), "d");
        chain.clear_typeahead();
        assert!(chain.typeahead_buffer().is_empty());
    }

    #[test]
    fn the_pointer_entered_latch_tracks_chain_entry() {
        let chain = MenuChain::new(HoverTimings::default());
        let _ = chain.open_root(Some(rect(0.0, 0.0, 100.0, 100.0)), OpenSource::Click);
        assert!(!chain.pointer_entered());
        chain.note_pointer_entered();
        assert!(chain.pointer_entered());
        chain.reset();
        assert!(!chain.pointer_entered());
        assert!(chain.open_levels().is_empty());
    }

    #[test]
    fn timings_default_to_the_design_values() {
        let timings = HoverTimings::default();
        assert_eq!(timings.open_delay.as_millis(), 200);
        assert_eq!(timings.close_grace.as_millis(), 300);
        assert_eq!(timings.typeahead_timeout.as_millis(), 2000);
    }

    #[test]
    fn the_open_source_separates_pointer_levels_from_kept_levels() {
        let chain = MenuChain::new(HoverTimings::default());
        let root = chain.open_root(None, OpenSource::Click);
        let pointer_child = chain.begin_open(root, 1, OpenSource::Pointer);
        let _ = chain.confirm_open(
            pointer_child,
            Some(rect(200.0, 0.0, 100.0, 80.0)),
            OpenSource::Pointer,
        );
        assert_eq!(chain.source(pointer_child), Some(OpenSource::Pointer));
        assert_eq!(chain.deepest_kept(), Some(root));
        let keyboard_child = chain.begin_open(root, 2, OpenSource::Keyboard);
        let _ = chain.confirm_open(
            keyboard_child,
            Some(rect(200.0, 100.0, 100.0, 80.0)),
            OpenSource::Keyboard,
        );
        assert_eq!(chain.deepest_kept(), Some(keyboard_child));
        assert_eq!(chain.active(root), None);
        let _ = chain.set_active(root, Some(1));
        assert_eq!(chain.active(root), Some(1));
        assert_eq!(chain.anchor(root), None);
        let _ = chain.set_anchor(root, rect(0.0, 0.0, 10.0, 10.0));
        assert_eq!(chain.anchor(root), Some(rect(0.0, 0.0, 10.0, 10.0)));
    }
    #[test]
    fn the_derived_safe_triangle_covers_the_narrow_band_at_zero_gap() {
        let anchor = rect(100.0, 100.0, 200.0, 30.0);
        let child = rect(300.0, 120.0, 120.0, 200.0);
        let triangle = derived_safe_triangle(anchor, child).expect("derived triangle");
        assert_eq!(
            triangle.points()[0],
            point(295.0, 115.0),
            "顶点按 -5px bleed 在行右缘左侧"
        );
        assert!(triangle.contains(point(298.0, 200.0)), "近缘窄带落点在内");
        assert!(triangle.contains(point(299.0, 125.0)));
        assert!(
            !triangle.contains(point(290.0, 200.0)),
            "父面板内侧落点在外"
        );
        assert!(
            !triangle.contains(point(298.0, 110.0)),
            "子菜单上方落点在外"
        );
    }

    #[test]
    fn the_derived_safe_triangle_mirrors_for_a_left_side_submenu() {
        let anchor = rect(300.0, 100.0, 200.0, 30.0);
        let child = rect(180.0, 120.0, 120.0, 200.0);
        let triangle = derived_safe_triangle(anchor, child).expect("derived triangle");
        assert_eq!(
            triangle.points()[0],
            point(305.0, 115.0),
            "顶点按 +5px bleed 在行左缘右侧"
        );
        assert!(triangle.contains(point(302.0, 200.0)), "镜像近缘窄带在内");
        assert!(triangle.contains(point(301.0, 125.0)));
        assert!(!triangle.contains(point(310.0, 200.0)));
    }
}

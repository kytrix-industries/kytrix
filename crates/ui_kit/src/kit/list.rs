use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch},
    app::App,
    error::EngineError,
    event::{Action, Modifiers},
    interaction::{
        ActionCtx, ItemAlign, ScrollHandle, VirtualList, VirtualListConfig, VirtualListState,
    },
    math::{LogicalPoint, LogicalPx, px},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::{NodeKind, NodeTree},
    },
    paint::{style::StylePatch, style_builder::StyleBuilder},
    window::{FrameIndex, WindowId},
};

use crate::{
    kit::scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
    theme::{handle::ThemeHandle, role::Role},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RowHeight {
    Fixed(LogicalPx),
    Variable { estimate: LogicalPx },
}

impl RowHeight {
    #[must_use]
    pub fn extent(self) -> LogicalPx {
        match self {
            Self::Fixed(extent) | Self::Variable { estimate: extent } => extent.max(px(1.0)),
        }
    }

    #[must_use]
    pub fn is_variable(self) -> bool {
        matches!(self, Self::Variable { .. })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionMode {
    #[default]
    None,
    Single,
    Multi,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListOptions {
    pub row_height: RowHeight,
    pub overscan: usize,
    pub selection: SelectionMode,
    pub capacity: Option<usize>,
    pub reverse: bool,
    pub keep_on_newest: bool,
    pub padding_x: LogicalPx,
    pub padding_y: LogicalPx,
    pub row_stretch: bool,
    pub scrollbar: bool,
    pub separator: bool,
    pub content_width: Option<LogicalPx>,
}

impl ListOptions {
    #[must_use]
    pub fn fixed(extent: LogicalPx) -> Self {
        Self {
            row_height: RowHeight::Fixed(extent),
            overscan: 2,
            selection: SelectionMode::Single,
            capacity: None,
            reverse: false,
            keep_on_newest: false,
            padding_x: px(6.0),
            padding_y: px(2.0),
            row_stretch: false,
            scrollbar: true,
            separator: false,
            content_width: None,
        }
    }

    #[must_use]
    pub fn variable(estimate: LogicalPx) -> Self {
        Self {
            row_height: RowHeight::Variable { estimate },
            ..Self::fixed(estimate)
        }
    }

    #[must_use]
    pub fn deque(mut self, capacity: usize) -> Self {
        self.capacity = Some(capacity.max(1));
        self
    }

    #[must_use]
    pub fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    #[must_use]
    pub fn keep_on_newest(mut self) -> Self {
        self.keep_on_newest = true;
        self
    }

    #[must_use]
    pub fn padding(mut self, x: LogicalPx, y: LogicalPx) -> Self {
        self.padding_x = x.max(LogicalPx::ZERO);
        self.padding_y = y.max(LogicalPx::ZERO);
        self
    }

    #[must_use]
    pub fn padding_x(mut self, padding: LogicalPx) -> Self {
        self.padding_x = padding.max(LogicalPx::ZERO);
        self
    }

    #[must_use]
    pub fn padding_y(mut self, padding: LogicalPx) -> Self {
        self.padding_y = padding.max(LogicalPx::ZERO);
        self
    }

    #[must_use]
    pub fn overscan(mut self, rows: usize) -> Self {
        self.overscan = rows;
        self
    }

    #[must_use]
    pub fn selection(mut self, mode: SelectionMode) -> Self {
        self.selection = mode;
        self
    }

    #[must_use]
    pub fn row_stretch(mut self) -> Self {
        self.row_stretch = true;
        self
    }

    #[must_use]
    pub fn separator(mut self) -> Self {
        self.separator = true;
        self
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Selection {
    indices: BTreeSet<usize>,
    anchor: Option<usize>,
}

impl std::fmt::Debug for Selection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Selection")
            .field("indices", &self.indices)
            .field("anchor", &self.anchor)
            .finish()
    }
}

impl Selection {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    #[must_use]
    pub fn contains(&self, index: usize) -> bool {
        self.indices.contains(&index)
    }

    #[must_use]
    pub fn first(&self) -> Option<usize> {
        self.indices.first().copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.indices.iter().copied()
    }

    pub fn clear(&mut self) {
        self.indices.clear();
        self.anchor = None;
    }

    pub fn retain_below(&mut self, count: usize) -> bool {
        let before = self.indices.len();
        self.indices.retain(|index| *index < count);
        if self.anchor.is_some_and(|anchor| anchor >= count) {
            self.anchor = None;
        }
        self.indices.len() != before
    }

    pub fn select(&mut self, index: usize) {
        self.indices.clear();
        self.indices.insert(index);
        self.anchor = Some(index);
    }

    pub fn toggle(&mut self, index: usize) {
        if !self.indices.remove(&index) {
            self.indices.insert(index);
        }
        self.anchor = Some(index);
    }

    pub fn select_range(&mut self, index: usize) {
        let Some(anchor) = self.anchor else {
            self.select(index);
            return;
        };
        let (low, high) = if anchor <= index {
            (anchor, index)
        } else {
            (index, anchor)
        };
        self.indices.clear();
        self.indices.extend(low..=high);
    }
}

pub(crate) fn apply_click(
    selection: &mut Selection,
    index: usize,
    mode: SelectionMode,
    ctrl: bool,
    shift: bool,
) {
    match mode {
        SelectionMode::None => {}
        SelectionMode::Single => selection.select(index),
        SelectionMode::Multi => {
            if shift {
                selection.select_range(index);
            } else if ctrl {
                selection.toggle(index);
            } else {
                selection.select(index);
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RowHeights {
    heights: Vec<LogicalPx>,
    prefix: Vec<LogicalPx>,
}

impl RowHeights {
    #[must_use]
    pub fn filled(count: usize, height: LogicalPx) -> Self {
        let height = height.max(LogicalPx::ZERO);
        let mut this = Self {
            heights: vec![height; count],
            prefix: Vec::with_capacity(count + 1),
        };
        this.rebuild_prefix();
        this
    }

    fn rebuild_prefix(&mut self) {
        self.prefix.clear();
        self.prefix.resize(self.heights.len() + 1, LogicalPx::ZERO);
        let mut total = LogicalPx::ZERO;
        for (index, height) in self.heights.iter().copied().enumerate() {
            total += height;
            self.prefix[index + 1] = total;
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.heights.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    #[must_use]
    pub fn as_slice(&self) -> &[LogicalPx] {
        &self.heights
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn get(&self, index: usize) -> Option<LogicalPx> {
        self.heights.get(index).copied()
    }

    #[must_use]
    pub fn top(&self, index: usize) -> LogicalPx {
        self.prefix
            .get(index)
            .copied()
            .unwrap_or_else(|| self.total())
    }

    #[must_use]
    pub fn total(&self) -> LogicalPx {
        self.prefix.last().copied().unwrap_or(LogicalPx::ZERO)
    }

    #[must_use]
    pub fn index_at(&self, offset: LogicalPx) -> (usize, LogicalPx) {
        if self.heights.is_empty() || self.total() <= LogicalPx::ZERO {
            return (0, offset.max(LogicalPx::ZERO));
        }
        let tops = &self.prefix[..self.heights.len()];
        let index = tops
            .partition_point(|top| *top <= offset)
            .saturating_sub(1)
            .min(self.heights.len() - 1);
        (index, (offset - tops[index]).max(LogicalPx::ZERO))
    }

    pub fn set(&mut self, index: usize, height: LogicalPx) -> bool {
        let height = height.max(LogicalPx::ZERO);
        let Some(slot) = self.heights.get_mut(index) else {
            return false;
        };
        if (*slot - height).abs() <= px(f32::EPSILON) {
            return false;
        }
        *slot = height;
        self.rebuild_prefix();
        true
    }

    pub fn insert_front(&mut self, height: LogicalPx) {
        self.heights.insert(0, height.max(LogicalPx::ZERO));
        self.rebuild_prefix();
    }

    pub fn push_back(&mut self, height: LogicalPx) {
        self.heights.push(height.max(LogicalPx::ZERO));
        self.rebuild_prefix();
    }

    pub fn pop_front(&mut self) -> Option<LogicalPx> {
        if self.heights.is_empty() {
            return None;
        }
        let removed = self.heights.remove(0);
        self.rebuild_prefix();
        Some(removed)
    }

    pub fn pop_back(&mut self) -> Option<LogicalPx> {
        let removed = self.heights.pop();
        if removed.is_some() {
            self.rebuild_prefix();
        }
        removed
    }

    pub fn resize(&mut self, count: usize, fallback: LogicalPx) {
        if self.heights.len() == count {
            return;
        }
        self.heights.resize(count, fallback.max(LogicalPx::ZERO));
        self.rebuild_prefix();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PushAnchor {
    pub index: usize,
    pub inner: LogicalPx,
    pub at_newest: bool,
}

#[must_use]
pub(crate) fn push_anchor_after(
    anchor: PushAnchor,
    full: bool,
    reverse: bool,
    new_count: usize,
) -> Option<(usize, LogicalPx)> {
    if new_count == 0 {
        return None;
    }
    let new_index = if reverse {
        anchor.index.saturating_add(1)
    } else if full {
        anchor.index.saturating_sub(1)
    } else {
        anchor.index
    };
    Some((new_index.min(new_count - 1), anchor.inner))
}

pub type RowContent = dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>;

fn merge_a11y(base: &mut A11yPatch, over: A11yPatch) {
    macro_rules! cover {
        ($($field:ident),+ $(,)?) => {
            $(
                if over.$field.is_some() {
                    base.$field = over.$field;
                }
            )+
        };
    }
    cover!(
        role,
        label,
        value,
        description,
        disabled,
        hidden,
        row_index,
        column_index,
        row_count,
        column_count,
        checked,
        selected,
        expanded,
        has_popup,
        live,
        pos_in_set,
        size_of_set,
        numeric_value,
        min_numeric_value,
        max_numeric_value,
        numeric_value_step,
        numeric_value_jump,
        actions,
        key,
        sort,
        selection,
        custom_actions,
        read_only,
        modal,
        busy,
        invalid,
        error_message,
        placeholder,
        level,
    );
}

#[derive(Clone)]
pub struct List {
    inner: Rc<ListInner>,
}

struct ListInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: ListOptions,
    content: Rc<RowContent>,
    scope: SignalScope,
    selection: Signal<Selection>,
    hovered: Signal<Option<usize>>,
    count: Signal<usize>,
    disabled: RefCell<BTreeSet<usize>>,
    row_a11y: RefCell<Option<Rc<dyn Fn(&mut ReadCtx<'_>, usize) -> A11yPatch>>>,
    heights: RefCell<RowHeights>,
    pending_text_measure: Cell<Option<FrameIndex>>,
    geometry: RefCell<Geometry>,
    select_action: Action,
    row_action: RefCell<Option<Action>>,
    rows_tab_stop: Cell<bool>,
    activate: RefCell<Option<Rc<dyn Fn(&mut App, usize)>>>,
    scrollbar: ScrollBar,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    outer: Option<NodeId>,
    viewport: Option<NodeId>,
    list: Option<VirtualList>,
    handle: Option<ScrollHandle>,
}

impl List {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        options: ListOptions,
        content: impl Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError> + 'static,
    ) -> Self {
        let key = key.into();
        let scrollbar = ScrollBar::new(
            app,
            theme.clone(),
            format_smolstr!("{key}:scrollbar"),
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default(),
        );
        let scope = app.signal_scope();
        Self {
            inner: Rc::new(ListInner {
                scope,
                selection: app.signal_scoped(scope, Selection::new()),
                hovered: app.signal_scoped(scope, None),
                count: app.signal_scoped(scope, 0usize),
                disabled: RefCell::new(BTreeSet::new()),
                row_a11y: RefCell::new(None),
                heights: RefCell::new(RowHeights::default()),
                pending_text_measure: Cell::new(None),
                geometry: RefCell::new(Geometry::default()),
                content: Rc::new(content),
                select_action: Action::Custom(format_smolstr!("{key}.select")),
                row_action: RefCell::new(None),
                rows_tab_stop: Cell::new(true),
                activate: RefCell::new(None),
                scrollbar,
                theme,
                options,
                key,
            }),
        }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    pub fn release(&self, app: &mut App) -> usize {
        app.off_action(&self.inner.select_action);
        self.inner.scrollbar.release(app) + app.release_scope(self.inner.scope)
    }

    pub(crate) fn set_disabled_rows(&self, rows: impl IntoIterator<Item = usize>) {
        *self.inner.disabled.borrow_mut() = rows.into_iter().collect();
    }

    pub(crate) fn set_rows_tab_stop(&self, tab_stop: bool) {
        self.inner.rows_tab_stop.set(tab_stop);
    }

    #[must_use]
    pub(crate) fn is_disabled(&self, index: usize) -> bool {
        self.inner.disabled.borrow().contains(&index)
    }

    pub fn set_row_a11y(&self, row_a11y: impl Fn(&mut ReadCtx<'_>, usize) -> A11yPatch + 'static) {
        *self.inner.row_a11y.borrow_mut() = Some(Rc::new(row_a11y));
    }

    pub fn build(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        if let Some(width) = self.inner.options.content_width
            && (!width.is_finite() || width < LogicalPx::ZERO)
        {
            return Err(EngineError::InvalidContentWidth { node: parent });
        }
        let key = &self.inner.key;
        let outer_style = StyleBuilder::new()
            .relative()
            .size_full()
            .min_width_0()
            .min_height_0()
            .overflow_hidden()
            .build();
        let outer = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:outer"))
            .style(outer_style)
            .build(tree, parent)?;
        let viewport_style = StyleBuilder::new()
            .relative()
            .size_full()
            .min_width_0()
            .min_height_0()
            .overflow_hidden()
            .build();
        let count = self.inner.count;
        let viewport = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:viewport"))
            .style(viewport_style)
            .on_click(self.inner.select_action.clone())
            .bind_a11y(move |cx| A11yPatch {
                size_of_set: Some(*cx.read(&count)),
                ..A11yPatch::default()
            })
            .build(tree, outer)?;
        if self.inner.options.scrollbar {
            self.inner.scrollbar.build(tree, outer)?;
        }
        let mut geometry = self.inner.geometry.borrow_mut();
        geometry.outer = Some(outer);
        geometry.viewport = Some(viewport);
        Ok(outer)
    }

    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let (Some(viewport), Some(outer)) = (
            self.inner.geometry.borrow().viewport,
            self.inner.geometry.borrow().outer,
        ) else {
            return Ok(());
        };
        let _ = outer;
        let options = &self.inner.options;
        let list = app.make_virtual_list(
            window,
            viewport,
            VirtualListConfig {
                extent: options.row_height.extent(),
                extents: None,
                overscan: options.overscan,
                content_width: options.content_width,
            },
        )?;
        let count = self.count(app);
        self.inner.ensure_heights(count);
        app.set_virtual_items(&list, count, self.row_builder())?;
        let handle = app.scroll_handle(window, viewport);
        {
            let mut geometry = self.inner.geometry.borrow_mut();
            geometry.window = Some(window);
            geometry.list = Some(list);
            geometry.handle = handle;
        }
        if options.scrollbar {
            self.inner.scrollbar.attach(app, window, viewport)?;
        }
        let select = self.clone();
        app.on_action(self.inner.select_action.clone(), move |ctx| {
            select.on_click(ctx);
        });
        self.populate_first_frame(app, window)?;
        Ok(())
    }

    pub fn mount(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let outer = {
            let Some(target) = app.window_mut(window) else {
                return Err(EngineError::UnknownWindow { window });
            };
            self.build(target.tree_mut(), parent)?
        };
        self.attach(app, window)?;
        Ok(outer)
    }

    #[must_use]
    pub fn outer_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().outer
    }

    #[must_use]
    pub fn viewport_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().viewport
    }

    #[must_use]
    pub fn count(&self, app: &App) -> usize {
        app.read_with(&self.inner.count, |count| *count)
    }

    #[must_use]
    pub fn count_signal(&self) -> Signal<usize> {
        self.inner.count
    }

    pub fn set_initial_count(&self, app: &mut App, count: usize) {
        self.inner.ensure_heights(count);
        app.write(&self.inner.count, count);
    }

    #[must_use]
    pub fn selection(&self, app: &App) -> Selection {
        app.read_with(&self.inner.selection, Selection::clone)
    }

    #[must_use]
    pub fn selection_signal(&self) -> Signal<Selection> {
        self.inner.selection
    }

    #[must_use]
    pub fn hovered_signal(&self) -> Signal<Option<usize>> {
        self.inner.hovered
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn row_heights(&self) -> RowHeights {
        self.inner.heights.borrow().clone()
    }

    #[must_use]
    pub fn state(&self, app: &App) -> Option<VirtualListState> {
        let list = self.inner.geometry.borrow().list?;
        app.virtual_list_state(&list)
    }

    #[must_use]
    pub fn offset(&self, app: &App) -> Option<LogicalPx> {
        let handle = self.inner.geometry.borrow().handle?;
        app.scroll_state(&handle).map(|state| state.offset.y)
    }

    #[must_use]
    pub fn mounted_rows(&self, app: &App) -> Vec<(usize, NodeId)> {
        let geometry = self.inner.geometry.borrow();
        let (Some(window), Some(viewport)) = (geometry.window, geometry.viewport) else {
            return Vec::new();
        };
        drop(geometry);
        let Some(target) = app.window(window) else {
            return Vec::new();
        };
        let tree = target.tree();
        let Ok(container) = tree.get(viewport) else {
            return Vec::new();
        };
        let mut rows: Vec<(usize, NodeId)> = container
            .children()
            .iter()
            .filter_map(|row| {
                let id = tree.debug_id(*row)?;
                parse_row_index(&self.inner.key, id).map(|index| (index, *row))
            })
            .collect();
        rows.sort_unstable_by_key(|(index, _)| *index);
        rows
    }

    pub fn select(&self, app: &mut App, index: Option<usize>) -> bool {
        if index.is_some_and(|index| self.is_disabled(index)) {
            return false;
        }
        let mut selection = Selection::new();
        if let Some(index) = index {
            selection.select(index);
        }
        app.write(&self.inner.selection, selection)
    }

    pub fn clear_selection(&self, app: &mut App) -> bool {
        app.write(&self.inner.selection, Selection::new())
    }

    pub fn on_activate(&self, callback: impl Fn(&mut App, usize) + 'static) {
        *self.inner.activate.borrow_mut() = Some(Rc::new(callback));
    }

    pub(crate) fn set_row_action(&self, action: Action) {
        *self.inner.row_action.borrow_mut() = Some(action);
    }

    pub fn set_items(&self, app: &mut App, count: usize) -> Result<(), EngineError> {
        self.inner.ensure_heights(count);
        self.rebuild(app, count, Some(LogicalPx::ZERO))?;
        app.write(&self.inner.selection, Selection::new());
        Ok(())
    }

    pub fn set_count(&self, app: &mut App, count: usize) -> Result<(), EngineError> {
        self.inner.ensure_heights(count);
        let offset = self.offset(app).unwrap_or(LogicalPx::ZERO);
        self.rebuild(app, count, Some(offset))?;
        app.update(&self.inner.selection, |selection| {
            selection.retain_below(count);
        });
        Ok(())
    }

    pub fn push(&self, app: &mut App) -> Result<(), EngineError> {
        let capacity = self.inner.options.capacity.unwrap_or(usize::MAX).max(1);
        let old_count = self.count(app);
        let full = old_count >= capacity;
        let new_count = (old_count + 1).min(capacity);
        let Some(handle) = self.inner.geometry.borrow().handle else {
            return Ok(());
        };
        let Some(state) = app.scroll_state(&handle) else {
            return Ok(());
        };
        let viewport = state.viewport.height;
        let offset = state.offset.y;
        let (first, inner) = self.inner.index_at(offset, old_count);
        let max = (self.inner.content_height(old_count) - viewport).max(LogicalPx::ZERO);
        let at_newest = if self.inner.options.reverse {
            offset <= px(0.5)
        } else {
            max <= px(0.5) || offset >= max - px(0.5)
        };
        let anchor = PushAnchor {
            index: first,
            inner,
            at_newest,
        };
        let estimate = self.inner.options.row_height.extent();
        if self.inner.options.row_height.is_variable() {
            let mut heights = self.inner.heights.borrow_mut();
            if self.inner.options.reverse {
                heights.insert_front(estimate);
                if full {
                    heights.pop_back();
                }
            } else {
                if full {
                    heights.pop_front();
                }
                heights.push_back(estimate);
            }
            heights.resize(new_count, estimate);
        }
        let follow = self.inner.options.keep_on_newest && at_newest;
        let target = if follow {
            if self.inner.options.reverse {
                LogicalPx::ZERO
            } else {
                (self.inner.content_height(new_count) - viewport).max(LogicalPx::ZERO)
            }
        } else if let Some((index, inner)) =
            push_anchor_after(anchor, full, self.inner.options.reverse, new_count)
        {
            self.inner.row_top(index, new_count) + inner
        } else {
            LogicalPx::ZERO
        };
        self.rebuild(app, new_count, Some(target))
    }

    pub fn scroll_to_item(
        &self,
        app: &mut App,
        index: usize,
        align: ItemAlign,
    ) -> Result<(), EngineError> {
        let Some(list) = self.inner.geometry.borrow().list else {
            return Ok(());
        };
        app.scroll_to_item(&list, index, align)
    }

    pub fn scroll_by(&self, app: &mut App, delta: LogicalPx) -> Result<(), EngineError> {
        let Some(handle) = self.inner.geometry.borrow().handle else {
            return Ok(());
        };
        app.scroll_by(&handle, LogicalPoint::new(LogicalPx::ZERO, delta))
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let Some(window) = self.inner.geometry.borrow().window else {
            return Ok(false);
        };
        let mut changed = false;
        let rows = self.mounted_rows(app);
        let populated_now = self.fill_row_contents(app, window)?;
        if !populated_now.is_empty() {
            changed = true;
        }
        let hovered = self.row_index_at_node(app, app.hovered(window));
        if app.write(&self.inner.hovered, hovered) {
            changed = true;
        }
        if self.inner.options.row_height.is_variable() {
            let pending = self.inner.pending_text_measure.get();
            let defer_text = pending.is_some_and(|pending| {
                app.window(window)
                    .is_some_and(|target| target.frame_index() == pending)
            });
            if pending.is_some() && !defer_text {
                self.inner.pending_text_measure.set(None);
            }
            let mut measured = false;
            {
                let Some(target) = app.window(window) else {
                    return Ok(changed);
                };
                let tree = target.tree();
                let padding = self.inner.options.padding_y;
                let mut heights = self.inner.heights.borrow_mut();
                for &(index, row) in &rows {
                    if populated_now.contains(&index) {
                        continue;
                    }
                    let Some(content) = tree
                        .get(row)
                        .ok()
                        .and_then(|node| node.children().first().copied())
                    else {
                        continue;
                    };
                    if defer_text && subtree_contains_text(tree, content) {
                        continue;
                    }
                    let height = tree
                        .get(content)
                        .map(|node| node.layout().bounds.size.height)
                        .unwrap_or(LogicalPx::ZERO)
                        + padding * 2.0;
                    if heights.set(index, height) {
                        measured = true;
                    }
                }
            }
            if measured {
                let heights: Vec<LogicalPx> = self.inner.heights.borrow().as_slice().to_vec();
                if let Some(list) = self.inner.geometry.borrow().list {
                    app.set_virtual_extents(&list, &heights)?;
                }
                changed = true;
            }
        }
        if self.inner.scrollbar.sync(app)? {
            changed = true;
        }
        Ok(changed)
    }

    fn fill_row_contents(
        &self,
        app: &mut App,
        window: WindowId,
    ) -> Result<Vec<usize>, EngineError> {
        let rows = self.mounted_rows(app);
        let mut populated: Vec<usize> = Vec::new();
        for &(index, row) in &rows {
            let needs_content = app
                .window(window)
                .and_then(|target| target.tree().get(row).ok())
                .is_some_and(|node| node.children().is_empty());
            if !needs_content {
                continue;
            }
            let content = self.inner.content.clone();
            let content_id = format_smolstr!("{}:row:{}:content", self.inner.key, index);
            let content_style = StyleBuilder::new()
                .flex_row()
                .width_full()
                .min_width_0()
                .min_height_0()
                .build();
            let Some(target) = app.window_mut(window) else {
                break;
            };
            let tree = target.tree_mut();
            let parent = NodeBuilder::boxed()
                .id(content_id)
                .style(content_style)
                .build(tree, row)?;
            content(tree, parent, index)?;
            if self.inner.options.separator && index > 0 {
                let theme = self.inner.theme.clone();
                NodeBuilder::boxed()
                    .id(format_smolstr!(
                        "{}:row:{}:separator",
                        self.inner.key,
                        index
                    ))
                    .style(
                        StyleBuilder::new()
                            .absolute()
                            .top(px(0.0))
                            .left(px(0.0))
                            .right(px(0.0))
                            .height(px(1.0))
                            .build(),
                    )
                    .bind_style(move |cx| StylePatch {
                        background: Some(theme.role(cx, Role::LineDivider)),
                        ..StylePatch::default()
                    })
                    .a11y(A11yOverride {
                        hidden: true,
                        ..A11yOverride::default()
                    })
                    .build(tree, row)?;
            }
            populated.push(index);
        }
        for &(index, row) in &rows {
            if populated.contains(&index) {
                app.request_node_repaint(window, row)?;
            }
        }
        Ok(populated)
    }

    #[must_use]
    pub fn row_node(&self, app: &App, index: usize) -> Option<NodeId> {
        self.mounted_rows(app)
            .into_iter()
            .find_map(|(mounted, node)| (mounted == index).then_some(node))
    }

    #[must_use]
    pub fn row_content_node(&self, app: &App, index: usize) -> Option<NodeId> {
        let row = self.row_node(app, index)?;
        let window = self.inner.geometry.borrow().window?;
        let tree = app.window(window)?.tree();
        tree.get(row).ok()?.children().first().copied()
    }

    fn row_index_at_node(&self, app: &App, start: Option<NodeId>) -> Option<usize> {
        let geometry = self.inner.geometry.borrow();
        let (Some(window), Some(viewport), Some(list)) =
            (geometry.window, geometry.viewport, geometry.list)
        else {
            return None;
        };
        drop(geometry);
        let mut cursor = start;
        let tree = app.window(window)?.tree();
        while let Some(node) = cursor {
            if let Some(index) = app.virtual_row_index(&list, node) {
                return Some(index);
            }
            if node == viewport {
                return None;
            }
            cursor = tree.get(node).ok().and_then(|target| target.parent());
        }
        None
    }

    fn on_click(&self, ctx: &mut ActionCtx<'_>) {
        let hit_node = ctx.hit().map(|hit| hit.node);
        if hit_node.is_none() {
            let selected = self.selection(ctx.app_mut()).first();
            if let Some(index) = selected
                && !self.is_disabled(index)
            {
                let callback = self.inner.activate.borrow().clone();
                if let Some(callback) = callback {
                    callback(ctx.app_mut(), index);
                }
            }
            return;
        }
        let Some(index) = self.row_index_at_node(ctx.app_mut(), hit_node) else {
            return;
        };
        if self.is_disabled(index) {
            return;
        }
        let mode = self.inner.options.selection;
        if mode != SelectionMode::None {
            let ctrl = ctx.modifiers().contains(Modifiers::CONTROL);
            let shift = ctx.modifiers().contains(Modifiers::SHIFT);
            ctx.update(&self.inner.selection, |selection| {
                apply_click(selection, index, mode, ctrl, shift);
            });
        }
        let click_count = match ctx.source() {
            ui_core::event::ActionSource::Click { click_count, .. } => click_count,
            _ => 1,
        };
        if click_count >= 2 {
            let callback = self.inner.activate.borrow().clone();
            if let Some(callback) = callback {
                callback(ctx.app_mut(), index);
            }
        }
    }

    fn row_builder(&self) -> impl FnMut(usize) -> NodeBuilder + 'static {
        let key = self.inner.key.clone();
        let theme = self.inner.theme.clone();
        let selection = self.inner.selection;
        let hovered = self.inner.hovered;
        let count = self.inner.count;
        let options = self.inner.options.clone();
        let inner = self.inner.clone();
        move |index| {
            let row_style = StyleBuilder::new()
                .flex_row()
                .width_full()
                .min_width_0()
                .min_height_0()
                .padding_sides(
                    options.padding_y,
                    options.padding_x,
                    options.padding_y,
                    options.padding_x,
                )
                .overflow_hidden();
            let row_style = if options.row_stretch {
                row_style.build()
            } else {
                row_style.items_center().build()
            };
            let mut row_style = row_style;
            if let Some(content_width) = options.content_width {
                row_style.min_width(taffy::style::LengthPercentageAuto::length(
                    content_width.value(),
                ));
            }
            let row_theme = theme.clone();
            let a11y_inner = inner.clone();
            let mut row = NodeBuilder::boxed()
                .id(format_smolstr!("{key}:row:{index}"))
                .style(row_style)
                .tab_stop(inner.rows_tab_stop.get())
                .bind_style(move |cx| {
                    let selected = cx.read(&selection).contains(index);
                    let hovered = *cx.read(&hovered) == Some(index);
                    let background = if selected {
                        Some(row_theme.role(cx, Role::Selection))
                    } else if hovered {
                        Some(row_theme.role(cx, Role::VeilHover))
                    } else {
                        None
                    };
                    StylePatch {
                        background,
                        ..StylePatch::default()
                    }
                })
                .bind_a11y(move |cx| {
                    let mut patch = A11yPatch {
                        selected: Some(cx.read(&selection).contains(index)),
                        pos_in_set: Some(index),
                        size_of_set: Some(*cx.read(&count)),
                        disabled: Some(a11y_inner.disabled.borrow().contains(&index)),
                        ..A11yPatch::default()
                    };
                    let row_a11y = a11y_inner.row_a11y.borrow().clone();
                    if let Some(row_a11y) = row_a11y {
                        merge_a11y(&mut patch, row_a11y(cx, index));
                    }
                    patch
                });
            if let Some(action) = inner.row_action.borrow().clone() {
                row = row.on_click(action);
            }
            row
        }
    }

    fn rebuild(
        &self,
        app: &mut App,
        count: usize,
        target_offset: Option<LogicalPx>,
    ) -> Result<(), EngineError> {
        let window = self.inner.geometry.borrow().window;
        let (Some(list), Some(handle)) = (
            self.inner.geometry.borrow().list,
            self.inner.geometry.borrow().handle,
        ) else {
            app.write(&self.inner.count, count);
            return Ok(());
        };
        let heights: Option<Vec<LogicalPx>> = if self.inner.options.row_height.is_variable() {
            Some(self.inner.heights.borrow().as_slice().to_vec())
        } else {
            None
        };
        app.batch(|app| {
            app.set_virtual_items(&list, 0, self.row_builder())?;
            app.set_virtual_items(&list, count, self.row_builder())?;
            if let Some(heights) = heights.as_deref()
                && count > 0
            {
                app.set_virtual_extents(&list, heights)?;
            }
            if let Some(offset) = target_offset {
                app.scroll_to(&handle, LogicalPoint::new(LogicalPx::ZERO, offset))?;
            }
            Ok(())
        })?;
        app.write(&self.inner.count, count);
        if let Some(window) = window {
            let populated = self.fill_row_contents(app, window)?;
            if !populated.is_empty() && self.inner.options.row_height.is_variable() {
                settle_layout(app, window)?;
                self.inner
                    .pending_text_measure
                    .set(app.window(window).map(|target| target.frame_index()));
            }
        }
        Ok(())
    }

    fn populate_first_frame(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        {
            let Some(target) = app.window_mut(window) else {
                return Ok(());
            };
            target.compute_layout()?;
        }
        let count = self.count(app);
        let Some(list) = self.inner.geometry.borrow().list else {
            return Ok(());
        };
        app.set_virtual_items(&list, count, self.row_builder())?;
        let populated = self.fill_row_contents(app, window)?;
        if !populated.is_empty() && self.inner.options.row_height.is_variable() {
            settle_layout(app, window)?;
            self.inner
                .pending_text_measure
                .set(app.window(window).map(|target| target.frame_index()));
        }
        Ok(())
    }
}

fn settle_layout(app: &mut App, window: WindowId) -> Result<(), EngineError> {
    if let Some(target) = app.window_mut(window) {
        target.compute_layout()?;
    }
    Ok(())
}

fn subtree_contains_text(tree: &NodeTree, node: NodeId) -> bool {
    let Ok(target) = tree.get(node) else {
        return false;
    };
    if matches!(target.kind(), NodeKind::Text(_)) {
        return true;
    }
    target
        .children()
        .iter()
        .any(|child| subtree_contains_text(tree, *child))
}

impl ListInner {
    fn ensure_heights(&self, count: usize) {
        if !self.options.row_height.is_variable() {
            return;
        }
        let estimate = self.options.row_height.extent();
        let mut heights = self.heights.borrow_mut();
        if heights.is_empty() && count > 0 {
            *heights = RowHeights::filled(count, estimate);
        } else {
            heights.resize(count, estimate);
        }
    }

    fn row_top(&self, index: usize, count: usize) -> LogicalPx {
        match self.options.row_height {
            RowHeight::Fixed(extent) => extent * index.min(count) as f32,
            RowHeight::Variable { estimate } => {
                let heights = self.heights.borrow();
                if heights.is_empty() {
                    estimate * index as f32
                } else {
                    heights.top(index)
                }
            }
        }
    }

    fn content_height(&self, count: usize) -> LogicalPx {
        match self.options.row_height {
            RowHeight::Fixed(extent) => extent * count as f32,
            RowHeight::Variable { estimate } => {
                let heights = self.heights.borrow();
                if heights.is_empty() {
                    estimate * count as f32
                } else {
                    heights.total()
                }
            }
        }
    }

    fn index_at(&self, offset: LogicalPx, count: usize) -> (usize, LogicalPx) {
        if count == 0 {
            return (0, LogicalPx::ZERO);
        }
        match self.options.row_height {
            RowHeight::Fixed(extent) => {
                let index = ((offset / extent).floor().max(0.0) as usize).min(count - 1);
                (index, offset - extent * index as f32)
            }
            RowHeight::Variable { estimate } => {
                let heights = self.heights.borrow();
                if heights.is_empty() {
                    let index = ((offset / estimate).floor().max(0.0) as usize).min(count - 1);
                    (index, offset - estimate * index as f32)
                } else {
                    heights.index_at(offset)
                }
            }
        }
    }
}

#[cfg(test)]
#[must_use]
pub(crate) fn row_debug_id(key: &str, index: usize) -> SmolStr {
    format_smolstr!("{key}:row:{index}")
}

#[must_use]
pub(crate) fn parse_row_index(key: &str, id: &str) -> Option<usize> {
    id.strip_prefix(key)?.strip_prefix(":row:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_heights_prefix_and_index_at() {
        let mut heights = RowHeights::filled(4, px(25.0));
        assert_eq!(heights.total(), px(100.0));
        assert_eq!(heights.top(0), px(0.0));
        assert_eq!(heights.top(2), px(50.0));
        assert_eq!(heights.index_at(px(0.0)), (0, px(0.0)));
        assert_eq!(heights.index_at(px(24.0)), (0, px(24.0)));
        assert_eq!(heights.index_at(px(25.0)), (1, px(0.0)));
        assert_eq!(heights.index_at(px(74.0)), (2, px(24.0)));
        assert_eq!(
            heights.index_at(px(200.0)),
            (3, px(125.0)),
            "越界偏移退到末项（行内偏移可超过行高）"
        );

        assert!(heights.set(1, px(50.0)));
        assert_eq!(heights.total(), px(125.0));
        assert_eq!(heights.top(2), px(75.0));
        assert_eq!(heights.index_at(px(75.0)), (2, px(0.0)));
        assert!(!heights.set(1, px(50.0)), "同值不重建前缀和");

        let empty = RowHeights::default();
        assert_eq!(empty.index_at(px(12.0)), (0, px(12.0)), "空表无条目");
        assert_eq!(empty.total(), px(0.0));
    }

    #[test]
    fn row_heights_mutations_keep_the_prefix_consistent() {
        let mut heights = RowHeights::filled(3, px(10.0));
        heights.insert_front(px(5.0));
        assert_eq!(heights.as_slice(), &[px(5.0), px(10.0), px(10.0), px(10.0)]);
        assert_eq!(heights.top(1), px(5.0));
        assert_eq!(heights.total(), px(35.0));

        heights.pop_back();
        assert_eq!(heights.total(), px(25.0));
        heights.push_back(px(20.0));
        assert_eq!(heights.total(), px(45.0));
        heights.pop_front();
        assert_eq!(heights.top(0), px(0.0));
        assert_eq!(heights.total(), px(40.0));

        heights.resize(5, px(7.0));
        assert_eq!(
            heights.as_slice(),
            &[px(10.0), px(10.0), px(20.0), px(7.0), px(7.0)]
        );
        assert_eq!(heights.total(), px(54.0));
        heights.resize(2, px(7.0));
        assert_eq!(heights.total(), px(20.0));
    }
    #[test]
    fn selection_click_modes() {
        let mut selection = Selection::new();
        apply_click(&mut selection, 3, SelectionMode::Single, false, false);
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![3]);

        apply_click(&mut selection, 5, SelectionMode::Multi, false, false);
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![5]);
        apply_click(&mut selection, 7, SelectionMode::Multi, true, false);
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![5, 7]);
        apply_click(&mut selection, 7, SelectionMode::Multi, true, false);
        assert_eq!(
            selection.iter().collect::<Vec<_>>(),
            vec![5],
            "Ctrl 再点切换掉"
        );

        selection.select(2);
        apply_click(&mut selection, 5, SelectionMode::Multi, false, true);
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![2, 3, 4, 5]);
        assert_eq!(selection.first(), Some(2));

        apply_click(&mut selection, 9, SelectionMode::None, true, true);
        assert_eq!(selection.len(), 4, "None 模式不改选中");

        assert!(selection.retain_below(4));
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn push_anchor_moves_with_the_dropped_or_inserted_item() {
        let anchor = PushAnchor {
            index: 3,
            inner: px(4.0),
            at_newest: false,
        };
        assert_eq!(
            push_anchor_after(anchor, false, false, 5),
            Some((3, px(4.0)))
        );
        assert_eq!(
            push_anchor_after(anchor, true, false, 5),
            Some((2, px(4.0))),
            "非倒序满容量丢首项 ⇒ 读者下标 −1"
        );
        assert_eq!(
            push_anchor_after(anchor, true, true, 5),
            Some((4, px(4.0))),
            "倒序满容量头插 ⇒ 读者下标 +1"
        );
        assert_eq!(
            push_anchor_after(anchor, false, false, 3),
            Some((2, px(4.0))),
            "条目数缩小时 clamp 到末项"
        );
        assert_eq!(push_anchor_after(anchor, false, false, 0), None);
    }

    #[test]
    fn row_ids_roundtrip() {
        assert_eq!(row_debug_id("list", 7), SmolStr::new_static("list:row:7"));
        assert_eq!(parse_row_index("list", "list:row:7"), Some(7));
        assert_eq!(parse_row_index("list", "list:row:7:content"), None);
        assert_eq!(parse_row_index("list", "other:row:7"), None);
    }
}

#[cfg(test)]
mod ui_tests {
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        a11y::{A11yPatch, A11yRole},
        error::EngineError,
        event::{InputEvent, Modifiers, PointerButton},
        interaction::{ScrollAxes, ScrollConfig, ScrollHandle},
        math::{LogicalPoint, LogicalPx, LogicalSize, px},
        node::{builder::NodeBuilder, dependency::ReadCtx, id::NodeId, tree::NodeTree},
        paint::style_builder::StyleBuilder,
        test_support::app::TestApp,
        text::TextStyle,
    };

    use super::{List, ListOptions, RowHeight, SelectionMode};
    use crate::{
        kit::test_support::a11y_of,
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    const BOX_WIDTH: f32 = 200.0;
    const BOX_HEIGHT: f32 = 120.0;
    const ROW_EXTENT: f32 = 30.0;

    struct Harness {
        app: TestApp,
        list: List,
    }

    fn text_row(tree: &mut NodeTree, parent: NodeId, index: usize) -> Result<(), EngineError> {
        NodeBuilder::text(format_smolstr!("row{index}"), TextStyle::new(px(12.0)))
            .build(tree, parent)?;
        Ok(())
    }

    fn harness_with(
        options: ListOptions,
        rows: usize,
        content: impl Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError> + 'static,
    ) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(app.app_mut(), theme, "list", options, content);
        mounted_harness(app, list, rows)
    }

    fn mounted_harness(mut app: TestApp, list: List, rows: usize) -> Harness {
        list.set_initial_count(app.app_mut(), rows);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(BOX_WIDTH))
                        .height(px(BOX_HEIGHT))
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");
        let mut harness = Harness { app, list };
        settle(&mut harness);
        harness
    }

    fn harness(rows: usize) -> Harness {
        harness_with(ListOptions::fixed(px(ROW_EXTENT)), rows, text_row)
    }

    fn settle(harness: &mut Harness) {
        for _ in 0..6 {
            let _ = harness.app.drain().expect("frame");
            let _ = harness.list.sync(harness.app.app_mut()).expect("sync");
        }
        let _ = harness.app.drain().expect("frame");
    }

    fn center(app: &TestApp, node: NodeId) -> LogicalPoint {
        let bounds = app.probe().bounds(node).expect("bounds");
        LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn click(app: &mut TestApp, point: LogicalPoint) {
        let _ = app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    fn click_outcome(app: &mut TestApp, point: LogicalPoint) -> ui_core::event::InputOutcome {
        let _ = app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ))
    }

    fn collect_texts(tree: &NodeTree, node: NodeId, out: &mut Vec<SmolStr>) {
        let Ok(target) = tree.get(node) else {
            return;
        };
        if let ui_core::node::tree::NodeKind::Text(content) = target.kind() {
            out.push(content.text.clone());
        }
        for child in target.children() {
            collect_texts(tree, *child, out);
        }
    }

    fn first_text_node(tree: &NodeTree, node: NodeId) -> Option<NodeId> {
        let target = tree.get(node).ok()?;
        for child in target.children() {
            let Ok(child_target) = tree.get(*child) else {
                continue;
            };
            if matches!(child_target.kind(), ui_core::node::tree::NodeKind::Text(_)) {
                return Some(*child);
            }
            if let Some(found) = first_text_node(tree, *child) {
                return Some(found);
            }
        }
        None
    }

    fn row_texts(
        app: &ui_core::app::App,
        window: ui_core::window::WindowId,
        list: &List,
        row: usize,
    ) -> Vec<SmolStr> {
        let tree = app.window(window).expect("window").tree();
        let Some(row) = list.row_node(app, row) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        collect_texts(tree, row, &mut out);
        out
    }

    #[test]
    fn mounting_populates_row_contents_before_the_first_frame() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(
            app.app_mut(),
            theme,
            "first-frame",
            ListOptions::fixed(px(ROW_EXTENT)),
            text_row,
        );
        list.set_initial_count(app.app_mut(), 20);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(BOX_WIDTH))
                        .height(px(BOX_HEIGHT))
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");

        let mounted = list.mounted_rows(app.app());
        assert_eq!(mounted.len(), 6, "首帧前必须按真实视口挂满可见窗口");
        for (index, _) in &mounted {
            assert_eq!(
                row_texts(app.app(), window, &list, *index),
                vec![format_smolstr!("row{index}")],
                "第 {index} 行内容必须在首帧前同步建好"
            );
        }

        let first = app.drain().expect("frame");
        assert!(first.rendered, "首帧必须渲染");
        assert_eq!(
            list.mounted_rows(app.app()).len(),
            6,
            "首帧不得再靠 rewindow 补挂新行（否则这些行首帧是空的）"
        );
        for (index, _) in list.mounted_rows(app.app()) {
            assert!(
                !row_texts(app.app(), window, &list, index).is_empty(),
                "首帧第 {index} 行不得是空行"
            );
        }

        for frame_number in 0..5 {
            let record = app.drain().expect("frame");
            assert!(
                !record.rendered,
                "首帧之后第 {} 帧不得再渲染（出帧循环）",
                frame_number + 2
            );
        }

        let row0 = list.row_node(app.app(), 0).expect("row 0");
        let text = first_text_node(app.app().window(window).expect("window").tree(), row0)
            .expect("第 0 行必须有文本节点");
        let bounds = app.probe().bounds(text).expect("行文本 bounds");
        assert!(
            bounds.size.width.value() > 0.0 && bounds.size.height.value() > 0.0,
            "首帧行文本必须已布局（bounds 非零）：{bounds:?}"
        );
    }

    #[test]
    fn releasing_the_list_removes_its_click_handler() {
        let mut harness = harness(5);
        let select_action = harness.list.inner.select_action.clone();
        assert!(
            harness.app.app().has_action_handler(&select_action),
            "attach 必须注册点击 handler"
        );
        let row0 = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let point0 = center(&harness.app, row0);
        let click0 = click_outcome(&mut harness.app, point0);
        assert_eq!(
            click0.action,
            Some(select_action.clone()),
            "释放前点击必须由列表 handler 消费"
        );
        assert_eq!(
            harness.list.selection(harness.app.app()).first(),
            Some(0),
            "释放前点击正常选中"
        );

        assert!(harness.list.release(harness.app.app_mut()) > 0);
        assert!(
            !harness.app.app().has_action_handler(&select_action),
            "release 必须摘掉点击 handler（否则 App 永久持有 Rc<List>）"
        );

        let row1 = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        let point1 = center(&harness.app, row1);
        let click1 = click_outcome(&mut harness.app, point1);
        assert_eq!(click1.action, None, "释放后重放点击不得再命中列表 handler");
    }

    #[test]
    fn rebuilding_items_populates_row_contents_in_the_same_call() {
        let mut harness = harness(10);
        harness
            .list
            .set_items(harness.app.app_mut(), 5)
            .expect("set items");
        assert_eq!(harness.list.count(harness.app.app()), 5);
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 0),
            vec![SmolStr::new_static("row0")],
            "set_items 返回时首行内容已挂载"
        );
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 4),
            vec![SmolStr::new_static("row4")]
        );
        let _ = harness.app.drain().expect("frame");
        let _ = harness.list.sync(harness.app.app_mut()).expect("sync");
        assert_eq!(harness.list.count(harness.app.app()), 5);
    }

    #[test]
    fn disabled_rows_reject_clicks_and_programmatic_selection() {
        let mut harness = harness(5);
        harness.list.set_disabled_rows([1usize]);
        let row = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        let point = center(&harness.app, row);
        click(&mut harness.app, point);
        assert!(
            harness.list.selection(harness.app.app()).is_empty(),
            "点击禁用行不选中"
        );
        let row = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let point = center(&harness.app, row);
        click(&mut harness.app, point);
        assert_eq!(harness.list.selection(harness.app.app()).first(), Some(0));
        assert!(
            !harness.list.select(harness.app.app_mut(), Some(1)),
            "程序化选中禁用行被拒绝"
        );
        assert_eq!(
            harness.list.selection(harness.app.app()).first(),
            Some(0),
            "拒绝后选中集不变"
        );
        assert!(harness.list.select(harness.app.app_mut(), Some(2)));
    }

    #[test]
    fn rows_declare_list_semantics_to_a11y() {
        let mut harness = harness(4);
        harness.app.enable_a11y_capture();
        let _ = harness.app.drain().expect("frame");
        let viewport = harness.list.viewport_node().expect("viewport");
        let row0 = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let row1 = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        {
            let snapshot = harness.app.a11y().expect("a11y snapshot");
            let container = snapshot.node_for(viewport).expect("viewport a11y");
            assert_eq!(container.role, ui_core::a11y::A11yRole::List);
            assert_eq!(container.size_of_set, Some(4), "列表宣告集合大小");
            let row = snapshot.node_for(row0).expect("row 0 a11y");
            assert_eq!(row.role, ui_core::a11y::A11yRole::ListBoxOption);
            assert_eq!(row.pos_in_set, Some(0));
            assert_eq!(row.size_of_set, Some(4));
            assert_eq!(row.selected, Some(false));
        }
        let point = center(&harness.app, row1);
        click(&mut harness.app, point);
        let _ = harness.app.drain().expect("frame");
        let snapshot = harness.app.a11y().expect("a11y snapshot");
        let row = snapshot.node_for(row1).expect("row 1 a11y");
        assert_eq!(row.selected, Some(true), "选中态进 a11y");
    }

    #[test]
    fn an_application_row_a11y_patch_overrides_the_row_baseline() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let label = app.app_mut().signal(SmolStr::new_static("Feed row"));
        let list = List::new(
            app.app_mut(),
            theme,
            "row-a11y",
            ListOptions::fixed(px(ROW_EXTENT)),
            text_row,
        );
        list.set_row_a11y(move |cx: &mut ReadCtx<'_>, index| A11yPatch {
            role: Some(A11yRole::Group),
            label: Some(format_smolstr!("{} #{index}", cx.read(&label))),
            ..A11yPatch::default()
        });
        let mut harness = mounted_harness(app, list, 4);
        harness.app.enable_a11y_capture();
        let _ = harness.app.drain().expect("frame");
        let row0 = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        {
            let snapshot = harness.app.a11y().expect("a11y snapshot");
            let row = a11y_of(snapshot, row0);
            assert_eq!(row.role, A11yRole::Group, "覆盖行基线的 ListBoxOption");
            assert_eq!(row.label_text(), Some("Feed row #0"));
            assert_eq!(row.pos_in_set, Some(0), "未覆盖的基线字段保留");
            assert_eq!(row.size_of_set, Some(4));
        }

        assert!(
            harness
                .app
                .app_mut()
                .write(&label, SmolStr::new_static("列表行"))
        );
        let _ = harness.app.drain().expect("a11y frame");
        assert_eq!(
            harness.list.row_node(harness.app.app(), 0),
            Some(row0),
            "行容器不重建"
        );
        let snapshot = harness.app.a11y().expect("a11y snapshot");
        assert_eq!(a11y_of(snapshot, row0).label_text(), Some("列表行 #0"));
    }

    #[test]
    fn a_plain_list_is_not_adopted_by_collection_semantics() {
        use ui_core::{a11y::A11yRole, interaction::ItemAlign};

        use crate::kit::test_support::{
            A11yUpdates, a11y_of, assert_single_parent_per_node, full_a11y,
        };

        let mut harness = harness(20);
        harness.app.enable_a11y_capture();
        let _ = harness.app.drain().expect("frame");
        harness.app.assert_a11y_contract();
        let viewport = harness.list.viewport_node().expect("viewport");
        let row0 = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        {
            let snapshot = harness.app.a11y().expect("a11y snapshot");
            let container = a11y_of(snapshot, viewport);
            assert_eq!(container.role, A11yRole::List);
            assert_eq!(container.size_of_set, Some(20));
            let row = a11y_of(snapshot, row0);
            assert_eq!(row.role, A11yRole::ListBoxOption);
            assert_eq!(
                row.parent,
                Some(container.id),
                "裸列表的行父是虚拟窗口（物理父子，未被集合角色领养）"
            );
            assert_eq!(row.pos_in_set, Some(0));
            assert_eq!(row.size_of_set, Some(20));
            for role in [
                A11yRole::Table,
                A11yRole::Grid,
                A11yRole::Row,
                A11yRole::Cell,
                A11yRole::ColumnHeader,
                A11yRole::Tree,
                A11yRole::TreeItem,
            ] {
                assert_eq!(
                    snapshot.nodes_with_role(role).count(),
                    0,
                    "纯列表不得出现集合角色 {role:?}"
                );
            }
        }
        let mut updates = A11yUpdates::new();
        updates.apply(&harness.app);
        harness
            .list
            .scroll_to_item(harness.app.app_mut(), 15, ItemAlign::Start)
            .expect("scroll");
        let _ = harness.app.drain().expect("scroll frame");
        harness.app.assert_a11y_contract();
        updates.apply(&harness.app);
        let snapshot = full_a11y(&mut harness.app);
        assert_single_parent_per_node(&snapshot);
        let container = a11y_of(&snapshot, viewport);
        assert_eq!(container.role, A11yRole::List);
        let mounted = harness.list.mounted_rows(harness.app.app());
        assert!(
            mounted.first().is_some_and(|(index, _)| *index > 0),
            "滚动后窗口换页"
        );
        for (index, row) in mounted {
            let node = a11y_of(&snapshot, row);
            assert_eq!(node.role, A11yRole::ListBoxOption);
            assert_eq!(node.parent, Some(container.id));
            assert_eq!(node.pos_in_set, Some(index), "位置是条目逻辑下标");
        }
        assert!(
            updates.applied() >= 2,
            "首笔与滚动笔都必须过 ShadowTree 契约"
        );
    }

    #[test]
    fn releasing_the_list_scope_reclaims_its_signal_slots() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(
            app.app_mut(),
            theme,
            "released",
            ListOptions::fixed(px(ROW_EXTENT)),
            text_row,
        );
        let released = list.release(app.app_mut());
        assert!(
            released >= 5,
            "三条列表信号 + 滚动条两条信号必须被回收（实际 {released}）"
        );
        assert_eq!(list.release(app.app_mut()), 0, "重复释放幂等");
    }

    #[test]
    fn a_fixed_list_mounts_only_the_visible_window() {
        let harness = harness(20);
        let mounted = harness.list.mounted_rows(harness.app.app());
        assert_eq!(mounted.len(), 6, "视口 120 / 行高 30 + overscan 2 ⇒ 6 行");
        let first = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let second = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        let first_bounds = harness.app.probe().bounds(first).expect("row 0 bounds");
        let second_bounds = harness.app.probe().bounds(second).expect("row 1 bounds");
        assert!(
            (second_bounds.origin.y - first_bounds.origin.y - ROW_EXTENT)
                .abs()
                .value()
                < 0.5,
            "行按 extent 紧凑排列：{first_bounds:?} {second_bounds:?}"
        );
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 2),
            vec![SmolStr::new_static("row2")]
        );
    }

    #[test]
    fn content_width_makes_the_list_dual_axis_with_wide_rows() {
        let mut options = ListOptions::fixed(px(ROW_EXTENT));
        options.content_width = Some(px(300.0));
        let harness = harness_with(options, 20, text_row);
        let viewport = harness.list.viewport_node().expect("viewport");
        let handle = harness
            .app
            .app()
            .scroll_handle(harness.app.window_id(), viewport)
            .expect("handle");
        let state = harness.app.app().scroll_state(&handle).expect("state");
        assert_eq!(state.axes, ScrollAxes::Both, "内容宽 > 视口 ⇒ 双轴");
        assert_eq!(state.viewport.width, px(BOX_WIDTH));
        assert_eq!(
            state.content.width,
            px(300.0),
            "横向内容宽 = ListOptions.content_width"
        );

        let row = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let bounds = harness.app.probe().bounds(row).expect("row bounds");
        assert_eq!(
            bounds.size.width,
            px(300.0),
            "挂载行必须撑到内容宽而不是视口宽：{bounds:?}"
        );
    }

    #[test]
    fn content_width_none_keeps_vertical_rows_at_the_viewport_width() {
        let harness = harness(20);
        let viewport = harness.list.viewport_node().expect("viewport");
        let handle = harness
            .app
            .app()
            .scroll_handle(harness.app.window_id(), viewport)
            .expect("handle");
        let state = harness.app.app().scroll_state(&handle).expect("state");
        assert_eq!(state.axes, ScrollAxes::Vertical, "None 保持纵向单轴");
        assert_eq!(
            state.content.width, state.viewport.width,
            "None：横向内容宽仍是视口宽"
        );

        let row = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let bounds = harness.app.probe().bounds(row).expect("row bounds");
        assert_eq!(
            bounds.size.width,
            px(BOX_WIDTH),
            "None：行宽 = 视口宽（旧行为）"
        );
    }

    #[test]
    fn invalid_content_width_is_rejected_by_the_list_build() {
        for width in [px(f32::NAN), px(-1.0)] {
            let mut app = TestApp::new();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let mut options = ListOptions::fixed(px(ROW_EXTENT));
            options.content_width = Some(width);
            let list = List::new(app.app_mut(), theme, "invalid", options, text_row);
            list.set_initial_count(app.app_mut(), 4);
            let window = app.window_id();
            let root = app.app().window(window).expect("window").tree().root();
            let parent = {
                let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
                NodeBuilder::boxed().build(tree, root).expect("parent")
            };
            let built = {
                let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
                list.build(tree, parent)
            };
            match built {
                Err(EngineError::InvalidContentWidth { .. }) => {}
                other => {
                    panic!("非法 content_width {width:?} 必须是 InvalidContentWidth：{other:?}")
                }
            }
        }
    }

    #[test]
    fn scrolling_moves_the_window_and_recycles_rows() {
        let mut harness = harness(20);
        harness
            .list
            .scroll_to_item(
                harness.app.app_mut(),
                10,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll to item");
        let _ = harness.app.drain().expect("frame");
        let _ = harness.list.sync(harness.app.app_mut()).expect("sync");
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some(px(300.0)),
            "第 10 行贴顶 = 10 * 30"
        );
        let mounted = harness.list.mounted_rows(harness.app.app());
        assert!(mounted.iter().any(|(index, _)| *index == 10));
        assert!(
            !mounted.iter().any(|(index, _)| *index == 0),
            "前缀行被回收"
        );
        assert_eq!(
            row_texts(
                harness.app.app(),
                harness.app.window_id(),
                &harness.list,
                10
            ),
            vec![SmolStr::new_static("row10")]
        );

        let viewport = harness.list.viewport_node().expect("viewport");
        let point = center(&harness.app, viewport);
        let _ = harness.app.inject(InputEvent::scroll_lines(
            point,
            LogicalPoint::new(px(0.0), px(-2.0)),
            Modifiers::empty(),
        ));
        for _ in 0..8 {
            harness.app.advance(std::time::Duration::from_millis(40));
            let _ = harness.app.drain().expect("frame");
        }
        let offset = harness.list.offset(harness.app.app()).expect("offset");
        assert!(offset > px(300.0), "滚轮下滚后偏移增大：{offset}");
    }

    #[test]
    fn scrolling_by_pixels_moves_the_window_and_clamps_at_both_ends() {
        let mut harness = harness(20);
        harness
            .list
            .scroll_by(harness.app.app_mut(), px(100.0))
            .expect("像素下滚");
        let _ = harness.app.drain().expect("frame");
        let _ = harness.list.sync(harness.app.app_mut()).expect("sync");
        assert_eq!(harness.list.offset(harness.app.app()), Some(px(100.0)));
        let mounted = harness.list.mounted_rows(harness.app.app());
        assert!(
            !mounted.iter().any(|(index, _)| *index == 0),
            "100 px 之后首行离开窗口"
        );
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 3),
            vec![SmolStr::new_static("row3")],
            "第 3 行进入窗口（100 / 30）"
        );

        harness
            .list
            .scroll_by(harness.app.app_mut(), px(-1_000.0))
            .expect("向上越界");
        let _ = harness.app.drain().expect("frame");
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some(px(0.0)),
            "负目标夹到 0，不产生负偏移"
        );

        harness
            .list
            .scroll_by(harness.app.app_mut(), px(10_000.0))
            .expect("向下越界");
        let _ = harness.app.drain().expect("frame");
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some(px(480.0)),
            "目标夹到 内容 600 − 视口 120"
        );
    }

    #[test]
    fn scrolling_by_pixels_uses_the_measured_heights() {
        let mut harness = harness_with(
            ListOptions::variable(px(ROW_EXTENT)),
            12,
            |tree, parent, index| {
                let height = if index.is_multiple_of(2) { 40.0 } else { 20.0 };
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width_full()
                            .height(px(height))
                            .flex_none()
                            .build(),
                    )
                    .build(tree, parent)?;
                Ok(())
            },
        );
        assert_eq!(harness.list.row_heights().get(0), Some(px(44.0)));
        let heights = harness.list.row_heights();
        let total = (0..12).fold(LogicalPx::ZERO, |total, index| {
            total + heights.get(index).unwrap_or(LogicalPx::ZERO)
        });
        assert_ne!(total, px(12.0 * ROW_EXTENT), "至少一行脱离了估计值");
        harness
            .list
            .scroll_by(harness.app.app_mut(), px(50.0))
            .expect("像素下滚");
        let _ = harness.app.drain().expect("frame");
        assert_eq!(harness.list.offset(harness.app.app()), Some(px(50.0)));

        harness
            .list
            .scroll_by(harness.app.app_mut(), px(10_000.0))
            .expect("向下越界");
        let _ = harness.app.drain().expect("frame");
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some((total - px(BOX_HEIGHT)).max(LogicalPx::ZERO)),
            "夹紧到实测内容 {total} − 视口 {BOX_HEIGHT}"
        );
    }

    #[test]
    fn scrolling_by_a_non_finite_delta_is_rejected() {
        let mut harness = harness(4);
        assert!(matches!(
            harness.list.scroll_by(harness.app.app_mut(), px(f32::NAN)),
            Err(EngineError::NotScrollable { .. })
        ));
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some(px(0.0)),
            "被拒后偏移不变"
        );
    }

    #[test]
    fn variable_heights_are_measured_and_written_back() {
        let mut harness = harness_with(
            ListOptions::variable(px(ROW_EXTENT)),
            20,
            |tree, parent, index| {
                let height = if index.is_multiple_of(2) { 40.0 } else { 20.0 };
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width_full()
                            .height(px(height))
                            .flex_none()
                            .build(),
                    )
                    .build(tree, parent)?;
                Ok(())
            },
        );
        settle(&mut harness);
        let heights = harness.list.row_heights();
        assert_eq!(heights.get(0), Some(px(44.0)), "40 + 上下 padding 2*2");
        assert_eq!(heights.get(1), Some(px(24.0)), "20 + padding");
        let first = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let second = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        let first_bounds = harness.app.probe().bounds(first).expect("bounds");
        let second_bounds = harness.app.probe().bounds(second).expect("bounds");
        assert!(
            (first_bounds.size.height - 44.0).abs().value() < 0.5,
            "{first_bounds:?}"
        );
        assert!(
            (second_bounds.origin.y - first_bounds.origin.y - 44.0)
                .abs()
                .value()
                < 0.5,
            "行位置跟前缀和走：{first_bounds:?} {second_bounds:?}"
        );
    }

    #[test]
    fn replacing_the_items_resets_the_scroll_and_the_selection() {
        let mut harness = harness(20);
        let row = harness.list.row_node(harness.app.app(), 2).expect("row 2");
        let point = center(&harness.app, row);
        click(&mut harness.app, point);
        assert_eq!(harness.list.selection(harness.app.app()).first(), Some(2));
        harness
            .list
            .scroll_to_item(
                harness.app.app_mut(),
                10,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll");
        let _ = harness.app.drain().expect("frame");
        harness
            .list
            .set_items(harness.app.app_mut(), 5)
            .expect("replace");
        settle(&mut harness);
        assert_eq!(harness.list.count(harness.app.app()), 5);
        assert_eq!(harness.list.offset(harness.app.app()), Some(px(0.0)));
        assert!(harness.list.selection(harness.app.app()).is_empty());
    }

    #[test]
    fn hover_is_mirrored_to_the_row_signal() {
        let mut harness = harness(10);
        let row = harness.list.row_node(harness.app.app(), 2).expect("row 2");
        let point = center(&harness.app, row);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(point, Modifiers::empty()));
        let _ = harness.app.drain().expect("frame");
        let _ = harness.list.sync(harness.app.app_mut()).expect("sync");
        let hovered = harness
            .app
            .app()
            .read_with(&harness.list.hovered_signal(), |hovered| *hovered);
        assert_eq!(hovered, Some(2), "悬停镜像到行下标");
    }

    #[test]
    fn an_idle_list_requests_no_frames() {
        let mut harness = harness(10);
        let _ = harness.app.drain().expect("frame");
        let record = harness.app.drain().expect("frame").clone();
        assert!(!record.rendered, "静止列表不得再出帧（I3）");
        assert!(
            !harness.list.sync(harness.app.app_mut()).expect("sync"),
            "无变化的 sync 不写状态"
        );
    }

    #[test]
    fn a_deque_push_drops_the_oldest_and_keeps_the_reader() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let data: Rc<RefCell<VecDeque<usize>>> = Rc::new(RefCell::new((0..8).collect()));
        let data_rows = data.clone();
        let list = List::new(
            app.app_mut(),
            theme,
            "deque",
            ListOptions::fixed(px(ROW_EXTENT)).deque(8).keep_on_newest(),
            move |tree, parent, index| {
                let value = data_rows.borrow().get(index).copied().unwrap_or(0);
                NodeBuilder::text(format_smolstr!("row{value}"), TextStyle::new(px(12.0)))
                    .build(tree, parent)?;
                Ok(())
            },
        );
        list.set_initial_count(app.app_mut(), 8);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(BOX_WIDTH))
                        .height(px(BOX_HEIGHT))
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");
        let mut harness = Harness { app, list };
        settle(&mut harness);
        harness
            .list
            .scroll_to_item(
                harness.app.app_mut(),
                2,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll");
        let _ = harness.app.drain().expect("frame");
        assert_eq!(harness.list.offset(harness.app.app()), Some(px(60.0)));
        {
            let mut data = data.borrow_mut();
            if data.len() == 8 {
                data.pop_front();
            }
            data.push_back(8);
        }
        harness.list.push(harness.app.app_mut()).expect("push");
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 0),
            vec![SmolStr::new_static("row1")],
            "push 返回时行内容已按新下标同步重建"
        );
        settle(&mut harness);
        assert_eq!(harness.list.count(harness.app.app()), 8);
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some(px(30.0)),
            "读者读的条目下标 −1，偏移跟着前缀和回退"
        );
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 0),
            vec![SmolStr::new_static("row1")],
            "最旧的一行被丢掉后，第 0 行显示原第 1 条"
        );
    }

    #[test]
    fn a_reversed_deque_push_prepends_and_keeps_the_reader() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let data: Rc<RefCell<VecDeque<usize>>> = Rc::new(RefCell::new((0..8).collect()));
        let data_rows = data.clone();
        let list = List::new(
            app.app_mut(),
            theme,
            "deque-reverse",
            ListOptions::fixed(px(ROW_EXTENT))
                .deque(8)
                .reverse()
                .keep_on_newest(),
            move |tree, parent, index| {
                let value = data_rows.borrow().get(index).copied().unwrap_or(0);
                NodeBuilder::text(format_smolstr!("row{value}"), TextStyle::new(px(12.0)))
                    .build(tree, parent)?;
                Ok(())
            },
        );
        list.set_initial_count(app.app_mut(), 8);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(BOX_WIDTH))
                        .height(px(BOX_HEIGHT))
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");
        let mut harness = Harness { app, list };
        settle(&mut harness);
        harness
            .list
            .scroll_to_item(
                harness.app.app_mut(),
                2,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll");
        let _ = harness.app.drain().expect("frame");
        assert_eq!(harness.list.offset(harness.app.app()), Some(px(60.0)));
        {
            let mut data = data.borrow_mut();
            if data.len() == 8 {
                data.pop_back();
            }
            data.push_front(8);
        }
        harness.list.push(harness.app.app_mut()).expect("push");
        settle(&mut harness);
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some(px(90.0)),
            "倒序头插 ⇒ 读者下标 +1，偏移前移一行"
        );
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 3),
            vec![SmolStr::new_static("row2")],
            "读者读的条目从旧下标 2 移到新下标 3（行内容随数据重排）"
        );
        assert_eq!(harness.list.count(harness.app.app()), 8);
        assert_eq!(
            harness.list.selection(harness.app.app()).len(),
            0,
            "push 不改选中"
        );
        let _ = SelectionMode::Single;
        let _ = RowHeight::Fixed(px(1.0));
    }

    fn variable_list_without_frames_with(
        options: ListOptions,
        rows: usize,
        content: impl Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError> + 'static,
    ) -> (TestApp, List) {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(app.app_mut(), theme, "real-pump", options, content);
        list.set_initial_count(app.app_mut(), rows);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(BOX_WIDTH))
                        .height(px(BOX_HEIGHT))
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");
        (app, list)
    }

    fn explicit_height_row(
        tree: &mut NodeTree,
        parent: NodeId,
        index: usize,
    ) -> Result<(), EngineError> {
        let height = if index.is_multiple_of(2) { 60.0 } else { 30.0 };
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .width_full()
                    .height(px(height))
                    .flex_none()
                    .build(),
            )
            .build(tree, parent)?;
        Ok(())
    }

    fn variable_list_without_frames() -> (TestApp, List) {
        const ROWS: usize = 8;
        const VARIABLE_EXTENT: f32 = 26.0;
        fn row_height(index: usize) -> f32 {
            if index.is_multiple_of(2) { 40.0 } else { 20.0 }
        }
        variable_list_without_frames_with(
            ListOptions::variable(px(VARIABLE_EXTENT)),
            ROWS,
            |tree: &mut NodeTree, parent: NodeId, index: usize| {
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width_full()
                            .height(px(row_height(index)))
                            .flex_none()
                            .build(),
                    )
                    .build(tree, parent)?;
                Ok::<(), EngineError>(())
            },
        )
    }

    fn assert_first_measurement_is_real(app: &TestApp, list: &List, measured: &[usize]) {
        const ROWS: usize = 8;
        fn real_height(index: usize) -> f32 {
            let content = if index.is_multiple_of(2) { 40.0 } else { 20.0 };
            content + 2.0 * 2.0
        }

        assert!(measured.len() >= 4, "首轮至少测量若干可见行：{measured:?}");
        let heights = list.row_heights();
        assert_eq!(heights.len(), ROWS, "逐项高度表长度 = 条目数");
        for &index in measured {
            assert_eq!(
                heights.get(index),
                Some(px(real_height(index))),
                "第 {index} 行高度（首轮必须读到实测真实高，而不是 0 / 估计值）"
            );
        }
        let state = list.state(app.app()).expect("虚拟列表状态");
        assert!(state.mounted > 0, "可见行必须挂载：{state:?}");
        assert!(
            heights.total() > px(0.0),
            "内容高度（前缀和）必须 > 0：{}",
            heights.total()
        );
    }

    #[test]
    fn the_first_sync_after_mount_measures_real_row_heights_without_a_frame_first() {
        let (mut app, list) = variable_list_without_frames();
        let measured: Vec<usize> = list
            .mounted_rows(app.app())
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        let changed = list.sync(app.app_mut()).expect("sync");
        assert!(changed, "首轮实测回写必须报告变化");
        assert_first_measurement_is_real(&app, &list, &measured);
    }

    #[test]
    fn the_first_sync_after_a_rebuild_measures_real_row_heights_without_a_frame_first() {
        let (mut app, list) = variable_list_without_frames();
        list.set_items(app.app_mut(), 8).expect("set items");
        let measured: Vec<usize> = list
            .mounted_rows(app.app())
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        let changed = list.sync(app.app_mut()).expect("sync");
        assert!(changed, "重建后首轮实测回写必须报告变化");
        assert_first_measurement_is_real(&app, &list, &measured);
    }

    #[test]
    fn the_first_sync_after_set_count_measures_real_heights_and_keeps_the_offset() {
        let (mut app, list) = variable_list_without_frames_with(
            ListOptions::variable(px(26.0)),
            5,
            explicit_height_row,
        );
        assert!(
            list.sync(app.app_mut()).expect("首轮 sync"),
            "首轮实测必须回写真实行高"
        );
        let before = list.row_heights();
        assert_eq!(before.get(0), Some(px(64.0)), "显式行高 60 + padding 2*2");
        assert_eq!(before.get(1), Some(px(34.0)), "显式行高 30 + padding 2*2");

        list.scroll_to_item(app.app_mut(), 2, ui_core::interaction::ItemAlign::Start)
            .expect("scroll");
        let offset = list.offset(app.app()).expect("offset");
        assert!(
            offset > px(0.0),
            "先滚到非零偏移（set_count 保留它的前提）：{offset}"
        );

        list.set_count(app.app_mut(), 5).expect("set count");
        let mounted: Vec<usize> = list
            .mounted_rows(app.app())
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        assert!(!mounted.is_empty(), "重建后必须挂载可见行");

        list.sync(app.app_mut())
            .expect("重建后首轮 sync（不先 drain）");
        let after = list.row_heights();
        for index in &mounted {
            let expected = if index.is_multiple_of(2) { 64.0 } else { 34.0 };
            assert_eq!(
                after.get(*index),
                Some(px(expected)),
                "第 {index} 行：set_count 后首轮实测必须是真实行高（不是 padding-only 4.0）"
            );
        }
        assert_eq!(
            list.offset(app.app()),
            Some(offset),
            "set_count 保留滚动偏移（首轮实测不得把它 clamp 掉）"
        );
    }

    #[test]
    fn the_first_sync_after_push_measures_real_heights_and_keeps_the_reader_anchor() {
        let data: Rc<RefCell<VecDeque<f32>>> =
            Rc::new(RefCell::new(VecDeque::from([60.0, 30.0, 60.0, 30.0, 60.0])));
        let rows = data.clone();
        let (mut app, list) = variable_list_without_frames_with(
            ListOptions::variable(px(26.0)).deque(5),
            5,
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                let height = rows.borrow().get(index).copied().unwrap_or(0.0);
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width_full()
                            .height(px(height))
                            .flex_none()
                            .build(),
                    )
                    .build(tree, parent)?;
                Ok::<(), EngineError>(())
            },
        );
        assert!(
            list.sync(app.app_mut()).expect("首轮 sync"),
            "首轮实测必须回写真实行高"
        );
        let before = list.row_heights();
        assert_eq!(before.get(0), Some(px(64.0)), "数据行高 60 + padding 2*2");
        assert_eq!(before.get(1), Some(px(34.0)), "数据行高 30 + padding 2*2");

        list.scroll_to_item(app.app_mut(), 2, ui_core::interaction::ItemAlign::Start)
            .expect("scroll");
        assert!(
            list.offset(app.app()).expect("offset") > px(0.0),
            "先滚到非零偏移（锚点保持的前提）"
        );

        {
            let mut data = data.borrow_mut();
            data.pop_front();
            data.push_back(30.0);
        }
        list.push(app.app_mut()).expect("push");
        let mounted: Vec<usize> = list
            .mounted_rows(app.app())
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        assert!(!mounted.is_empty(), "push 后必须挂载可见行");

        list.sync(app.app_mut())
            .expect("push 后首轮 sync（不先 drain）");
        let after = list.row_heights();
        let data = data.borrow();
        for index in &mounted {
            let content = data.get(*index).copied().expect("新数据覆盖已挂载行");
            assert_eq!(
                after.get(*index),
                Some(px(content + 4.0)),
                "第 {index} 行：push 后首轮实测必须是真实行高（不是 padding-only 4.0）"
            );
        }
        let offset = list.offset(app.app()).expect("offset");
        assert!(
            offset > px(0.0),
            "读者锚点仍在列表中部（没有被 clamp 到 0）：{offset}"
        );
        assert!(
            (offset - after.top(1)).abs() < px(0.5),
            "读者贴顶的条目丢首项后是下标 1：offset={offset} top(1)={}",
            after.top(1)
        );
    }

    #[test]
    fn set_count_on_a_text_row_height_list_keeps_the_scroll_offset() {
        let (mut app, list) = variable_list_without_frames_with(
            ListOptions::variable(px(26.0)),
            12,
            |tree: &mut NodeTree, parent: NodeId, index: usize| {
                NodeBuilder::text(format_smolstr!("row{index}"), TextStyle::new(px(12.0)))
                    .build(tree, parent)?;
                Ok::<(), EngineError>(())
            },
        );
        for _ in 0..3 {
            let _ = app.drain().expect("frame");
            let _ = list.sync(app.app_mut()).expect("sync");
        }
        let before = list.row_heights();
        let first = before.get(0).expect("第 0 行高度");
        assert!(
            (px(5.0)..px(40.0)).contains(&first),
            "文本行应有真实实测高度（不是 estimate / padding-only）：{first}"
        );

        list.scroll_to_item(app.app_mut(), 4, ui_core::interaction::ItemAlign::Start)
            .expect("scroll");
        let offset = list.offset(app.app()).expect("offset");
        assert!(offset > px(0.0), "先滚到非零偏移：{offset}");

        list.set_count(app.app_mut(), 12).expect("set count");
        list.sync(app.app_mut())
            .expect("重建后首轮 sync（不先 drain）");
        assert_eq!(
            list.row_heights().as_slice(),
            before.as_slice(),
            "首轮 sync 不得用 padding-only 高度覆盖已实测行高"
        );
        assert_eq!(
            list.offset(app.app()),
            Some(offset),
            "set_count 必须保留文本行高列表的滚动偏移（修好前实测 80 → 0）"
        );

        let _ = app.drain().expect("frame");
        list.sync(app.app_mut()).expect("帧后 sync");
        assert_eq!(
            list.offset(app.app()),
            Some(offset),
            "帧后补测量也不得移动读者"
        );
        let mounted: Vec<usize> = list
            .mounted_rows(app.app())
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        let after = list.row_heights();
        let real = after.get(mounted[0]).expect("挂载行高度");
        assert!(real > px(4.0), "挂载行必须补上真实高度：{after:?}");
        for index in &mounted {
            assert_eq!(
                after.get(*index),
                Some(real),
                "第 {index} 行：同文本文本行高应一致（且不是 padding-only）：{after:?}"
            );
        }
    }

    #[test]
    fn the_padding_builders_shape_the_row_boxes_and_measurements() {
        let options = ListOptions::fixed(px(ROW_EXTENT)).padding(px(3.0), px(5.0));
        assert_eq!(
            (options.padding_x, options.padding_y),
            (px(3.0), px(5.0)),
            "padding(x, y) 同时写两侧"
        );
        assert_eq!(
            ListOptions::fixed(px(ROW_EXTENT))
                .padding_x(px(1.5))
                .padding_x,
            px(1.5)
        );
        assert_eq!(
            ListOptions::fixed(px(ROW_EXTENT))
                .padding_y(px(2.5))
                .padding_y,
            px(2.5)
        );
        let clamped = ListOptions::fixed(px(ROW_EXTENT)).padding(px(-4.0), px(-2.0));
        assert_eq!(
            (clamped.padding_x, clamped.padding_y),
            (px(0.0), px(0.0)),
            "负内边距夹到 0"
        );
        assert_eq!(
            ListOptions::fixed(px(ROW_EXTENT)).overscan(5).overscan,
            5,
            "overscan builder 是行数口径"
        );

        let harness = harness_with(
            ListOptions::fixed(px(ROW_EXTENT))
                .padding(px(3.0), px(5.0))
                .row_stretch(),
            4,
            text_row,
        );
        let row = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let content = harness
            .list
            .row_content_node(harness.app.app(), 0)
            .expect("row 0 content");
        let row_bounds = harness.app.probe().bounds(row).expect("row bounds");
        let content_bounds = harness.app.probe().bounds(content).expect("content bounds");
        assert!(
            (content_bounds.origin.x - (row_bounds.origin.x + 3.0))
                .abs()
                .value()
                < 0.5,
            "内容左内边距 = padding_x（行 {row_bounds:?} / 内容 {content_bounds:?}）"
        );
        assert!(
            (content_bounds.origin.y - (row_bounds.origin.y + 5.0))
                .abs()
                .value()
                < 0.5,
            "内容上内边距 = padding_y（行 {row_bounds:?} / 内容 {content_bounds:?}）"
        );
        assert!(
            (content_bounds.size.width - (row_bounds.size.width - 6.0))
                .abs()
                .value()
                < 0.5,
            "内容可用宽 = 行宽 − 2×padding_x（行 {row_bounds:?} / 内容 {content_bounds:?}）"
        );

        let mut variable = harness_with(
            ListOptions::variable(px(10.0)).padding(px(3.0), px(5.0)),
            3,
            |tree: &mut NodeTree, parent: NodeId, index: usize| {
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width(px(40.0))
                            .height(px(20.0 + index as f32 * 10.0))
                            .build(),
                    )
                    .build(tree, parent)?;
                Ok::<(), EngineError>(())
            },
        );
        for _ in 0..3 {
            let _ = variable.app.drain().expect("frame");
            let _ = variable.list.sync(variable.app.app_mut()).expect("sync");
        }
        let heights = variable.list.row_heights();
        for index in 0..3 {
            assert_eq!(
                heights.get(index),
                Some(px(20.0 + index as f32 * 10.0 + 10.0)),
                "第 {index} 行 extent = 内容高 + 2 × padding_y"
            );
        }
    }

    struct PageHarness {
        app: TestApp,
        list: List,
        page: ScrollHandle,
    }

    fn page_with_list() -> PageHarness {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let root = app.app().window(window).expect("window").tree().root();
        let page = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .id("page")
                .style(
                    StyleBuilder::new()
                        .width(px(BOX_WIDTH))
                        .height(px(BOX_HEIGHT))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("page")
        };
        let page_handle = app
            .app_mut()
            .make_scroll(window, page, ScrollConfig::default())
            .expect("page scrollable");
        app.app_mut()
            .set_scroll_content(&page_handle, LogicalSize::new(px(BOX_WIDTH), px(400.0)))
            .expect("page content");
        let list = List::new(
            app.app_mut(),
            theme,
            "page-list",
            ListOptions::fixed(px(ROW_EXTENT)),
            text_row,
        );
        list.set_initial_count(app.app_mut(), 40);
        list.mount(app.app_mut(), window, page).expect("mount");
        let mut harness = PageHarness {
            app,
            list,
            page: page_handle,
        };
        settle_page(&mut harness);
        harness
    }

    fn settle_page(harness: &mut PageHarness) {
        for _ in 0..6 {
            harness.app.advance(std::time::Duration::from_millis(100));
            harness.app.drain().expect("frame");
            harness.list.sync(harness.app.app_mut()).expect("sync");
        }
        harness.app.drain().expect("frame");
    }

    fn wheel_over_node(harness: &mut PageHarness, node: NodeId) {
        let bounds = harness.app.probe().bounds(node).expect("node bounds");
        let point = LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        assert_eq!(
            harness.app.probe().hit(point).map(|hit| hit.node),
            Some(node),
            "滚轮落点必须命中 track/thumb 本身：布局漂移不得让用例假通过"
        );
        let _outcome = harness.app.inject(InputEvent::scroll_lines(
            point,
            LogicalPoint::new(px(0.0), px(-1.0)),
            Modifiers::empty(),
        ));
        settle_page(harness);
    }

    fn page_and_list_offsets(harness: &PageHarness) -> (LogicalPx, LogicalPx) {
        let page = harness
            .app
            .app()
            .scroll_state(&harness.page)
            .expect("page state")
            .offset
            .y;
        let viewport = harness.list.viewport_node().expect("list viewport");
        let handle = harness
            .app
            .app()
            .scroll_handle(harness.app.window_id(), viewport)
            .expect("list handle");
        let list = harness
            .app
            .app()
            .scroll_state(&handle)
            .expect("list state")
            .offset
            .y;
        (page, list)
    }

    #[test]
    fn list_wheel_over_track_scrolls_list_not_page() {
        let mut harness = page_with_list();
        let (track, _) = harness.list.inner.scrollbar.nodes();
        let track = track.expect("track node");
        wheel_over_node(&mut harness, track);
        let (page, list) = page_and_list_offsets(&harness);
        println!("kit: track wheel page_offset={page:.1} list_offset={list:.1}");
        assert_eq!(page, px(0.0), "滚轮落在列表滚动条 track 上不得滚外层页面");
        assert_eq!(list, px(48.0), "滚轮必须滚列表自身");
    }

    #[test]
    fn list_wheel_over_thumb_scrolls_list() {
        let mut harness = page_with_list();
        let (_, thumb) = harness.list.inner.scrollbar.nodes();
        let thumb = thumb.expect("thumb node");
        wheel_over_node(&mut harness, thumb);
        let (page, list) = page_and_list_offsets(&harness);
        println!("kit: thumb wheel page_offset={page:.1} list_offset={list:.1}");
        assert_eq!(page, px(0.0), "滚轮落在列表滚动条 thumb 上不得滚外层页面");
        assert_eq!(list, px(48.0), "滚轮必须滚列表自身");
    }
}

#[cfg(test)]
mod deque_tests {
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        error::EngineError,
        interaction::ItemAlign,
        math::px,
        node::{builder::NodeBuilder, tree::NodeTree},
        paint::style_builder::StyleBuilder,
        test_support::app::TestApp,
        text::TextStyle,
    };

    use super::{List, ListOptions};
    use crate::theme::{appearance::Appearance, handle::ThemeHandle};

    const BOX_WIDTH: f32 = 200.0;
    const BOX_HEIGHT: f32 = 120.0;
    const ROW_EXTENT: f32 = 30.0;

    fn mount_list(app: &mut TestApp, list: &List, rows: usize, window: ui_core::window::WindowId) {
        list.set_initial_count(app.app_mut(), rows);
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(BOX_WIDTH))
                        .height(px(BOX_HEIGHT))
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");
        settle(app, list);
    }

    fn settle(app: &mut TestApp, list: &List) {
        for _ in 0..6 {
            let _ = app.drain().expect("frame");
            let _ = list.sync(app.app_mut()).expect("sync");
        }
        let _ = app.drain().expect("frame");
    }

    fn fixed_deque(app: &mut TestApp, reverse: bool, data: Rc<RefCell<VecDeque<usize>>>) -> List {
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let mut options = ListOptions::fixed(px(ROW_EXTENT)).deque(8).keep_on_newest();
        if reverse {
            options = options.reverse();
        }
        let rows = data;
        List::new(
            app.app_mut(),
            theme,
            "deque",
            options,
            move |tree, parent, index| {
                let value = rows.borrow().get(index).copied().unwrap_or(0);
                NodeBuilder::text(format_smolstr!("row{value}"), TextStyle::new(px(12.0)))
                    .build(tree, parent)?;
                Ok(())
            },
        )
    }

    #[test]
    fn keeping_on_newest_follows_the_tail_and_leaves_a_scrolled_away_reader() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let data: Rc<RefCell<VecDeque<usize>>> = Rc::new(RefCell::new((0..8).collect()));
        let list = fixed_deque(&mut app, false, data.clone());
        mount_list(&mut app, &list, 8, window);

        list.scroll_to_item(app.app_mut(), 7, ItemAlign::End)
            .expect("scroll end");
        let _ = app.drain().expect("frame");
        let tail = list.offset(app.app()).expect("offset");
        {
            let mut data = data.borrow_mut();
            data.pop_front();
            data.push_back(8);
        }
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        assert_eq!(
            list.offset(app.app()),
            Some(tail),
            "读者在最新一条上 ⇒ 跟随尾部"
        );

        list.scroll_to_item(app.app_mut(), 2, ItemAlign::Start)
            .expect("scroll mid");
        let _ = app.drain().expect("frame");
        assert_eq!(list.offset(app.app()), Some(px(60.0)));
        {
            let mut data = data.borrow_mut();
            data.pop_front();
            data.push_back(9);
        }
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        assert_eq!(
            list.offset(app.app()),
            Some(px(30.0)),
            "滚走的读者不被跟随拖走：原第 2 条移到下标 1"
        );
        assert_eq!(list.count(app.app()), 8);
    }

    #[test]
    fn scrolling_by_pixels_leaves_and_rejoins_the_newest_reader() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let data: Rc<RefCell<VecDeque<usize>>> = Rc::new(RefCell::new((0..8).collect()));
        let list = fixed_deque(&mut app, false, data.clone());
        mount_list(&mut app, &list, 8, window);
        list.scroll_to_item(app.app_mut(), 7, ItemAlign::End)
            .expect("scroll end");
        let _ = app.drain().expect("frame");
        assert_eq!(list.offset(app.app()), Some(px(120.0)));
        list.scroll_by(app.app_mut(), px(-ROW_EXTENT))
            .expect("scroll up");
        let _ = app.drain().expect("frame");
        assert_eq!(list.offset(app.app()), Some(px(90.0)), "滚离尾部一行");
        {
            let mut data = data.borrow_mut();
            data.pop_front();
            data.push_back(8);
        }
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        assert_eq!(
            list.offset(app.app()),
            Some(px(60.0)),
            "读者读的那一条（原下标 3）移到 2：push 保持读者"
        );

        list.scroll_by(app.app_mut(), px(10_000.0))
            .expect("scroll to tail");
        let _ = app.drain().expect("frame");
        assert_eq!(list.offset(app.app()), Some(px(120.0)), "回到尾部");
        {
            let mut data = data.borrow_mut();
            data.pop_front();
            data.push_back(9);
        }
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        assert_eq!(
            list.offset(app.app()),
            Some(px(120.0)),
            "回到最新一条 ⇒ push 跟随尾部"
        );
    }

    #[test]
    fn a_reversed_deque_puts_a_reader_on_the_newest_item_back_on_top() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let data: Rc<RefCell<VecDeque<usize>>> = Rc::new(RefCell::new((0..8).collect()));
        let list = fixed_deque(&mut app, true, data.clone());
        mount_list(&mut app, &list, 8, window);
        assert_eq!(list.offset(app.app()), Some(px(0.0)));
        {
            let mut data = data.borrow_mut();
            data.pop_back();
            data.push_front(8);
        }
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        assert_eq!(
            list.offset(app.app()),
            Some(px(0.0)),
            "倒序列表的读者在最新一条上 ⇒ 回到顶部（最新一条）"
        );
        let node = list.row_node(app.app(), 0).expect("row 0");
        let window_id = app.window_id();
        let tree = app.app().window(window_id).expect("window").tree();
        let mut texts = Vec::new();
        fn walk(tree: &NodeTree, node: ui_core::node::id::NodeId, out: &mut Vec<SmolStr>) {
            let Ok(target) = tree.get(node) else {
                return;
            };
            if let ui_core::node::tree::NodeKind::Text(content) = target.kind() {
                out.push(content.text.clone());
            }
            for child in target.children() {
                walk(tree, *child, out);
            }
        }
        walk(tree, node, &mut texts);
        assert_eq!(
            texts,
            vec![SmolStr::new_static("row8")],
            "第 0 行是最新一条"
        );
    }

    #[test]
    fn a_variable_height_deque_keeps_the_reader_on_the_measured_prefix() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(
            app.app_mut(),
            theme,
            "deque-variable",
            ListOptions::variable(px(ROW_EXTENT))
                .deque(8)
                .keep_on_newest(),
            |tree: &mut NodeTree, parent: ui_core::node::id::NodeId, index: usize| {
                let height = if index.is_multiple_of(2) { 40.0 } else { 20.0 };
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width_full()
                            .height(px(height))
                            .flex_none()
                            .build(),
                    )
                    .build(tree, parent)?;
                Ok::<(), EngineError>(())
            },
        );
        mount_list(&mut app, &list, 8, window);
        let heights = list.row_heights();
        assert_eq!(heights.len(), 8);
        list.scroll_to_item(app.app_mut(), 2, ItemAlign::Start)
            .expect("scroll");
        let _ = app.drain().expect("frame");
        let expected = heights.top(2);
        assert!((list.offset(app.app()).expect("offset") - expected).abs() < px(0.5));
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        let expected = heights.get(1).expect("height 1");
        let offset = list.offset(app.app()).expect("offset");
        let _ = expected;
        let measured = list.row_heights();
        assert!(
            (offset - measured.top(1)).abs() < px(0.5),
            "变高双端列表按测量前缀和保持锚点（原第 2 条移到下标 1 并贴顶）"
        );
        assert!(offset > px(0.0));
    }

    fn separator_list(options: ListOptions, rows: usize) -> (TestApp, List) {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(
            app.app_mut(),
            theme,
            "list",
            options,
            |tree: &mut NodeTree, parent: ui_core::node::id::NodeId, index: usize| {
                NodeBuilder::text(format_smolstr!("row{index}"), TextStyle::new(px(12.0)))
                    .build(tree, parent)?;
                Ok::<(), EngineError>(())
            },
        );
        let window = app.window_id();
        mount_list(&mut app, &list, rows, window);
        (app, list)
    }

    #[test]
    fn the_builtin_separator_draws_one_divider_per_row_except_the_first() {
        let (app, list) = separator_list(ListOptions::fixed(px(ROW_EXTENT)).separator(), 4);
        let theme = crate::theme::resolved::Theme::of(Appearance::Light);
        let window = app.window_id();
        let mounted = list.mounted_rows(app.app());
        assert!(mounted.len() >= 2, "至少挂两行才能看到分隔线");
        for (index, row) in mounted {
            let tree = app.app().window(window).expect("window").tree();
            let children = tree.get(row).expect("row").children().to_vec();
            if index == 0 {
                assert_eq!(children.len(), 1, "首行不画分隔线");
                continue;
            }
            assert_eq!(
                children.len(),
                2,
                "第 {index} 行 = 内容容器 + 分隔线（内容容器仍是首子）"
            );
            assert!(
                tree.get(children[0])
                    .expect("content")
                    .children()
                    .iter()
                    .any(|child| matches!(
                        tree.get(*child).expect("row text").kind(),
                        ui_core::node::tree::NodeKind::Text(_)
                    )),
                "首子仍是行内容"
            );
            let separator = children[1];
            let expected_id = format_smolstr!("list:row:{index}:separator");
            assert_eq!(tree.debug_id(separator), Some(expected_id.as_str()));
            let style = tree.get(separator).expect("separator").style();
            assert_eq!(
                style.inner().size.height.into_option(),
                Some(1.0),
                "分隔线 1 px 高"
            );
            assert_eq!(
                style.inner().position,
                taffy::style::Position::Absolute,
                "分隔线绝对定位（不参与行内容布局）"
            );
            assert_eq!(
                style.inner().inset.top,
                taffy::style::LengthPercentageAuto::length(0.0),
                "分隔线贴行顶部"
            );
            assert_eq!(
                tree.get(row).expect("row").style().inner().position,
                taffy::style::Position::Absolute,
                "虚拟行绝对定位 ⇒ 行容器是分隔线的包含块"
            );
            let row_bounds = app.probe().bounds(row).expect("row bounds");
            let separator_bounds = app.probe().bounds(separator).expect("separator bounds");
            assert_eq!(
                separator_bounds.size.height.value(),
                1.0,
                "分隔线几何高 1 px"
            );
            assert!(
                (separator_bounds.size.width - row_bounds.size.width)
                    .abs()
                    .value()
                    < 0.5,
                "分隔线横跨整行（行宽 {}，线宽 {}）",
                row_bounds.size.width,
                separator_bounds.size.width
            );
            assert!(
                (separator_bounds.origin.y - row_bounds.origin.y)
                    .abs()
                    .value()
                    < 0.5,
                "分隔线落在行顶边（行 y {}，线 y {}）",
                row_bounds.origin.y,
                separator_bounds.origin.y
            );
            let patch = tree.get(separator).expect("separator").style_patch();
            assert_eq!(
                patch.background,
                Some(theme.role(crate::theme::role::Role::LineDivider)),
                "分隔线颜色 = LineDivider"
            );
        }

        let (plain, plain_list) = separator_list(ListOptions::fixed(px(ROW_EXTENT)), 4);
        let window = plain.window_id();
        for (_, row) in plain_list.mounted_rows(plain.app()) {
            let tree = plain.app().window(window).expect("window").tree();
            assert_eq!(
                tree.get(row).expect("row").children().len(),
                1,
                "未开 separator 时行里只有内容容器"
            );
        }
    }
}

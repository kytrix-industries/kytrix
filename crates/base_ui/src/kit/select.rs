//! 选择器（P2-4c：旧 `crate::select` 的引擎化重建 + 多选 / 标签）。
//!
//! 旧 `Select` 是 `Dropdown` 的一层薄包装：把选中项的 label（可选图标）画进触发器、画一个随 open
//! 状态旋转的 caret，菜单条目点击时写回选中并回调 `on_change`。新实现沿用同一结构，但把
//! **选中态、多选、清空、标签**都落在引擎信号上：
//!
//! ```text
//! 触发器（Interactive）: [标签 / 文本] [清空 X] [caret]
//! 面板（ItemPanel, ListBox 语义）: 选项行 = 文本 + 勾选标记
//! ```
//!
//! # 单选 / 多选
//!
//! - 单选（`multiple(false)`，默认）：点击 / 回车选中一行、回调、关闭面板（旧语义）；
//!   **重复选中同一项不回调**（旧实现的同 id 过滤），面板照常关闭；
//! - 多选（`multiple(true)`）：点击 / 回车切换一行的选中态，**面板不关**（继续勾选），
//!   选中集在触发器上以标签展示（[`SelectOptions::max_tags`] 个，超出显示 `+N`）。
//!
//! # 图标 / 宽度
//!
//! - 条目支持图标（旧 `Select` 的条目是 `MenuItem`，可带 `IconAction`）；单选选中后触发器
//!   画出选中项图标（多选画首项图标）；
//! - 面板宽度对齐触发器实测宽，但它是**下限**：内容更宽时面板长到内容宽（旧 `min_w` 口径）。
//!
//! 标签的视觉与 [`crate::kit::badge`] 的中性色块同源（`SurfaceControl` 底 / `LineBorder` 边 /
//! `FgDefault` 字 / 4 px 圆角 / 尺寸档高度），但 `Badge` 的文本是**建树期固定**的，而标签文本随
//! 选中集变化 —— 引擎的节点子树没有「重绑文本」入口，所以标签槽用 `bind_text` + `bind_style`
//! 复刻同一份配色与几何（等价性由测试守护；建议后续给 `Badge` 加动态文本通道，见 spec-delta）。
//!
//! # 键盘 / 关闭
//!
//! 触发器上 `↓` / `↑` 打开面板；面板内 `↑` / `↓` / `Home` / `End` 移动高亮、`Enter` / `Space`
//! 切换 / 选中、`Esc` 关闭（引擎默认 `Action::Dismiss`）、点击外部关闭（浮层 `OutsideClick`）。
//!
//! # 搜索过滤
//!
//! 旧实现**没有**搜索框（评估 §1.6），本批不新增（AGENTS §1.2：不做没有实际需求的扩展点）；
//! 需要搜索时由调用方用 `set_items` 过滤后重设条目。登记见 spec-delta。
//!
//! # a11y
//!
//! 引擎没有 `ComboBox` / `ListBox` 角色：触发器用 `Button`（可访问名 = 显式 label / 选中项文本 /
//! 多选摘要 `首项 +N` / 占位文本，随选中失效），展开态走 `aria-expanded`；面板行 `ListBoxOption`，
//! 集合语义由内层 E9 虚拟列表容器（恒 `List`）承担 —— 包装**不**重复声明 `List`（双 List 会让 AT
//! 读到「列表套列表」）。角色缺位与虚拟容器行 a11y 快照登记引擎批 E15（spec-delta）。

use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::{Dimension, Display};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, KeyBinding, KeyCode, Modifiers},
    interaction::ActionCtx,
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit, signal::Signal, tree::NodeTree},
    paint::{primitive::Border, style::StylePatch},
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};

use crate::{
    kit::{
        icon::IconKind,
        interactive::{Interactive, mount_dynamic_icon},
        layer::{Band, Layer},
        menu::{
            IconCache, ItemPanel, ItemPanelOptions, PanelContent, PanelMeasure, PanelRoles,
            PanelRow, PanelWidth, build_icon_slot,
        },
        overlay::{Anchor, AnimationRouter, OverlayError, Placement},
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{components::InteractiveVariant, handle::ThemeHandle, role::Role},
};

/// 标签圆角（与 `kit::badge` 一致的 4 px）。
const TAG_RADIUS: f32 = 4.0;
/// 标签之间的间隙（逻辑像素）。
const TAG_GAP: f32 = 4.0;
/// 勾选标记列宽（逻辑像素，保留列宽以保证行对齐）。
const CHECK_COLUMN: f32 = 14.0;

/// 选择项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectItem {
    /// 条目 id（回调里报告给调用方）。
    pub id: SmolStr,
    /// 显示文本（同时是可访问名）。
    pub label: SmolStr,
    /// 条目图标（旧 `Select` 的条目就是 `MenuItem`，支持 `IconAction`）；有图标时整列留槽，
    /// 单选选中后触发器也画该图标。
    pub icon: Option<IconKind>,
    /// 禁用（点击不选中）。
    pub disabled: bool,
}

impl SelectItem {
    /// 选项。
    #[must_use]
    pub fn new(id: impl Into<SmolStr>, label: impl Into<SmolStr>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            disabled: false,
        }
    }

    /// 图标。
    #[must_use]
    pub fn icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    /// 禁用。
    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// 选择器参数。
#[derive(Clone, Debug, PartialEq)]
pub struct SelectOptions {
    /// 尺寸档。
    pub size: UnifiedSize,
    /// 触发器视觉变体。
    pub variant: InteractiveVariant,
    /// 触发器 / 面板宽度（`None` = 触发器随内容、面板对齐触发器）。
    pub width: Option<f32>,
    /// 面板最大高度。
    pub max_height: f32,
    /// 面板定位。
    pub placement: Placement,
    /// 与触发器的间隙。
    pub gap: f32,
    /// 触发器可访问名（`None` 用选中文本 / 占位）。
    pub label: Option<SmolStr>,
    /// 空选中时的占位文本。
    pub placeholder: SmolStr,
    /// 是否多选。
    pub multiple: bool,
    /// 是否显示清空按钮。
    pub clearable: bool,
    /// 多选标签个数上限（超出用 `+N` 表示）。
    pub max_tags: usize,
}

impl Default for SelectOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            variant: InteractiveVariant::Outline,
            width: None,
            max_height: 320.0,
            placement: Placement::BOTTOM_START,
            gap: 4.0,
            label: None,
            placeholder: SmolStr::new_static("Select…"),
            multiple: false,
            clearable: false,
            max_tags: 2,
        }
    }
}

impl SelectOptions {
    /// 尺寸档。
    #[must_use]
    pub fn size(mut self, size: UnifiedSize) -> Self {
        self.size = size;
        self
    }

    /// 触发器变体。
    #[must_use]
    pub fn variant(mut self, variant: InteractiveVariant) -> Self {
        self.variant = variant;
        self
    }

    /// 显式宽度。
    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    /// 面板最大高度。
    #[must_use]
    pub fn max_height(mut self, max_height: f32) -> Self {
        self.max_height = max_height;
        self
    }

    /// 面板定位。
    #[must_use]
    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// 占位文本。
    #[must_use]
    pub fn placeholder(mut self, placeholder: impl Into<SmolStr>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// 触发器可访问名。
    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// 多选。
    #[must_use]
    pub fn multiple(mut self, multiple: bool) -> Self {
        self.multiple = multiple;
        self
    }

    /// 显示清空按钮。
    #[must_use]
    pub fn clearable(mut self, clearable: bool) -> Self {
        self.clearable = clearable;
        self
    }

    /// 多选标签上限。
    #[must_use]
    pub fn max_tags(mut self, max_tags: usize) -> Self {
        self.max_tags = max_tags.max(1);
        self
    }
}

/// 选择器（`Rc` 浅拷贝）。
#[derive(Clone)]
pub struct Select {
    inner: Rc<SelectInner>,
}

#[derive(Default)]
struct SelectState {
    window: Option<WindowId>,
    trigger: Option<NodeId>,
    trigger_width: f32,
    /// 上次写进标签槽的选中集（变化时才重排显示，I3）。
    tags_written: Vec<usize>,
    slots: Vec<Option<NodeId>>,
    overflow: Option<NodeId>,
    /// 触发器前导图标节点（当前选中项有图标时显示）。
    trigger_icon: Option<NodeId>,
    /// 当前图标节点上的图标（`None` = 尺寸 0 的占位）。
    icon_shown: Option<IconKind>,
}

struct SelectActions {
    trigger_click: Action,
    down: Action,
    up: Action,
    clear: Action,
}

struct SelectInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: SelectOptions,
    trigger: Interactive,
    panel: ItemPanel,
    icons: IconCache,
    items: Rc<RefCell<Rc<Vec<SelectItem>>>>,
    /// 条目内容修订号（`set_items` 自增；`bind_text` 读它触发重算）。
    revision: Signal<u64>,
    chosen: Signal<Vec<usize>>,
    /// 触发器可访问名（显式 label / 选中项文本 / 多选摘要 / 占位文本）。
    a11y_label: Signal<SmolStr>,
    on_change: RefCell<Option<Rc<dyn Fn(&mut App, Vec<SmolStr>)>>>,
    actions: SelectActions,
    state: RefCell<SelectState>,
}

impl Select {
    /// 建选择器（关闭态）。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
        options: SelectOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        let trigger_click = Action::Custom(format_smolstr!("{key}.trigger"));
        let panel = ItemPanel::new(
            app,
            theme.clone(),
            layer,
            router,
            format_smolstr!("{key}:panel"),
            ItemPanelOptions {
                size: options.size,
                max_height: options.max_height,
                row_height: options.size.h(),
                band: Band::Popup,
                modal: true,
                placement: options.placement,
                gap: options.gap,
                roles: PanelRoles {
                    // 包装不声明 `List`：内层 E9 虚拟列表容器已经恒为 `List`（双 List 会被
                    // AT 读成「列表套列表」）；`ListBox` 角色引擎尚未提供（E15 登记）。
                    panel: None,
                    row: A11yRole::ListBoxOption,
                },
                label: options.label.clone(),
                width: PanelWidth::AtLeast,
                slot: crate::kit::layer::LayerSlot::Exclusive,
            },
        );
        let open_state = panel.open_state();
        let static_label = options
            .label
            .clone()
            .unwrap_or_else(|| options.placeholder.clone());
        let a11y_label = app.signal_scoped(scope, static_label.clone());
        let trigger = Interactive::new(app, theme.clone(), format_smolstr!("{key}:trigger"))
            .variant(options.variant)
            .with_unified_size(options.size)
            .a11y_role(A11yRole::Button)
            .label(static_label)
            .a11y_label_signal(a11y_label)
            .a11y_expanded(open_state)
            .action(trigger_click.clone());
        let inner = Rc::new(SelectInner {
            actions: SelectActions {
                trigger_click,
                down: Action::Custom(format_smolstr!("{key}.open-down")),
                up: Action::Custom(format_smolstr!("{key}.open-up")),
                clear: Action::Custom(format_smolstr!("{key}.clear")),
            },
            key,
            theme,
            options,
            trigger,
            panel,
            icons: IconCache::default(),
            items: Rc::new(RefCell::new(Rc::new(Vec::new()))),
            revision: app.signal_scoped(scope, 0u64),
            chosen: app.signal_scoped(scope, Vec::new()),
            a11y_label,
            on_change: RefCell::new(None),
            state: RefCell::new(SelectState::default()),
        });
        let select = Self { inner };
        select.install_handlers(app);
        select
    }

    /// 选择器键（调试 id / 动作前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 触发器节点（`build` 之后）。
    #[must_use]
    pub fn trigger_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().trigger
    }

    /// 浮层面板节点（打开时）。
    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.panel.overlay().panel_node()
    }

    /// 打开状态信号。
    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.panel.open_state()
    }

    /// 是否打开。
    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        self.inner.panel.is_open(app)
    }

    /// 选中条目 id（按条目顺序）。
    #[must_use]
    pub fn chosen(&self, app: &App) -> Vec<SmolStr> {
        let items = self.inner.items.borrow();
        app.read_with(&self.inner.chosen, |chosen| {
            chosen
                .iter()
                .filter_map(|index| items.get(*index).map(|item| item.id.clone()))
                .collect()
        })
    }

    /// 选中条目下标。
    #[must_use]
    pub fn chosen_indices(&self, app: &App) -> Vec<usize> {
        app.read_with(&self.inner.chosen, Vec::clone)
    }

    /// 高亮行（面板打开时）。
    #[must_use]
    pub fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.panel.highlighted(app)
    }

    /// 条目快照。
    #[must_use]
    pub fn items(&self) -> Rc<Vec<SelectItem>> {
        self.inner.items.borrow().clone()
    }

    /// 换一批条目（选中集按 id 保留，找不到的丢弃）。
    ///
    /// # Errors
    /// 引擎虚拟列表失效。
    pub fn set_items(
        &self,
        app: &mut App,
        items: impl IntoIterator<Item = SelectItem>,
    ) -> Result<(), EngineError> {
        let previous = self.chosen(app);
        *self.inner.items.borrow_mut() = Rc::new(items.into_iter().collect());
        app.write(
            &self.inner.revision,
            app.read_with(&self.inner.revision, |revision| *revision) + 1,
        );
        let chosen: Vec<usize> = previous.iter().filter_map(|id| self.index_of(id)).collect();
        app.write(&self.inner.chosen, chosen);
        if let Some(window) = self.inner.state.borrow().window {
            self.resolve_icons(app, window)?;
        }
        self.sync_trigger_a11y(app);
        self.inner.panel.set_content(app, self.panel_content(app))
    }

    /// 程序化选中（`notify = false` 时不回调）。
    pub fn set_chosen<'a>(
        &self,
        app: &mut App,
        ids: impl IntoIterator<Item = &'a str>,
        notify: bool,
    ) {
        let mut chosen: Vec<usize> = ids.into_iter().filter_map(|id| self.index_of(id)).collect();
        chosen.sort_unstable();
        chosen.dedup();
        if !self.inner.options.multiple {
            chosen.truncate(1);
        }
        app.write(&self.inner.chosen, chosen);
        self.sync_trigger_a11y(app);
        if notify {
            self.notify(app);
        }
    }

    /// 清空选中（回调一次）。
    pub fn clear(&self, app: &mut App) -> bool {
        if !app.write(&self.inner.chosen, Vec::new()) {
            return false;
        }
        self.sync_trigger_a11y(app);
        self.notify(app);
        true
    }

    /// 选中变化回调（用户操作；`set_chosen(..., false)` / `clear` 不触发）。
    pub fn on_change(self, callback: impl Fn(&mut App, Vec<SmolStr>) + 'static) -> Self {
        *self.inner.on_change.borrow_mut() = Some(Rc::new(callback));
        self
    }

    /// 禁用 / 启用触发器。
    pub fn set_disabled(&self, app: &mut App, disabled: bool) -> bool {
        self.inner.trigger.set_disabled(app, disabled)
    }

    /// 建触发器节点（标签 / 文本 + 清空 + 箭头）。
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode` / 图标资源错误。
    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        self.inner.state.borrow_mut().window = Some(window);
        let size = self.inner.options.size;
        let multiple = self.inner.options.multiple;
        let clearable = self.inner.options.clearable;
        let placeholder = self.inner.options.placeholder.clone();
        let chosen = self.inner.chosen;
        let revision = self.inner.revision;
        let theme = self.inner.theme.clone();
        let trigger = self.inner.trigger.clone();
        let items = self.inner.items.clone();
        let slots_sink = Rc::new(RefCell::new(Vec::new()));
        let slots = slots_sink.clone();
        let overflow_sink = Rc::new(Cell::new(None));
        let overflow = overflow_sink.clone();
        let max_tags = self.inner.options.max_tags.max(1);
        let key = self.inner.key.clone();
        let clear_action = self.inner.actions.clear.clone();
        let trigger_watch = trigger.clone();
        // 触发器前导图标：当前选中项有图标时显示（旧 select 触发器画选中项图标）。
        let current_icon = self.selected_icon(app);
        self.inner.icons.resolve(
            app,
            window,
            current_icon.unwrap_or(IconKind::X),
            size.icon_size(),
        )?;
        let icon_resource = self
            .inner
            .icons
            .snapshot()
            .get(current_icon.unwrap_or(IconKind::X), size.icon_size())
            .ok_or(EngineError::UnknownWindow { window })?;
        let icon_slot = Rc::new(Cell::new(None));
        let icon_slot_inner = icon_slot.clone();
        let node = trigger.build(app, window, parent, move |app, window, parent| {
            let side = if current_icon.is_some() {
                size.icon_size()
            } else {
                0.0
            };
            let theme_icon = theme.clone();
            let icon = NodeBuilder::image(icon_resource, ImageFit::Fill)
                .style(StyleBuilder::new().size(side, side).build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_icon.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .a11y(A11yOverride {
                    hidden: true,
                    ..A11yOverride::default()
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    parent,
                )?;
            icon_slot_inner.set(Some(icon));
            // 文本（单选显示选中项；多选只在空选中时显示占位）。
            let theme_text = theme.clone();
            let text_placeholder = placeholder.clone();
            let text = NodeBuilder::text(
                SmolStr::new_static(""),
                TextStyle::new(size.text_size())
                    .with_wrap(TextWrap::NoWrap)
                    .with_overflow(TextOverflow::Ellipsis, 1),
            )
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_text({
                let items = items.clone();
                move |cx| {
                    let _ = *cx.read(&revision);
                    let chosen = cx.read(&chosen);
                    let items = items.borrow();
                    if multiple {
                        if chosen.is_empty() {
                            text_placeholder.clone()
                        } else {
                            SmolStr::new_static("")
                        }
                    } else {
                        chosen
                            .first()
                            .and_then(|index| items.get(*index))
                            .map(|item| item.label.clone())
                            .unwrap_or_else(|| text_placeholder.clone())
                    }
                }
            })
            .bind_style(move |cx| {
                let empty = cx.read(&chosen).is_empty();
                StylePatch {
                    foreground: Some(theme_text.role(
                        cx,
                        if empty {
                            Role::FgMuted
                        } else {
                            Role::FgDefault
                        },
                    )),
                    ..StylePatch::default()
                }
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
            trigger_watch.watch(text);
            if multiple {
                let row = NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .flex_row()
                            .items_center()
                            .relative()
                            .flex_1()
                            .min_width_0()
                            .gap(TAG_GAP)
                            .overflow_hidden()
                            .build(),
                    )
                    .a11y(A11yOverride {
                        hidden: true,
                        ..A11yOverride::default()
                    })
                    .build(
                        app.window_mut(window)
                            .ok_or(EngineError::UnknownWindow { window })?
                            .tree_mut(),
                        parent,
                    )?;
                let mut built = Vec::with_capacity(max_tags);
                for slot in 0..max_tags {
                    let chip = build_tag_chip(
                        app,
                        window,
                        TagChip {
                            parent: row,
                            size,
                            theme: &theme,
                            chosen,
                            revision,
                            items: &items,
                            key: &key,
                        },
                        slot,
                    )?;
                    built.push(Some(chip));
                }
                *slots.borrow_mut() = built;
                // `+N`（超出标签上限的选中数）。
                let theme_overflow = theme.clone();
                let overflow_text = NodeBuilder::text(
                    SmolStr::new_static(""),
                    TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
                )
                .style(hidden_style(StyleBuilder::new().flex_none().build()))
                .bind_text({
                    let items = items.clone();
                    move |cx| {
                        let _ = *cx.read(&revision);
                        let chosen = cx.read(&chosen);
                        let shown = chosen.len().min(max_tags);
                        if chosen.len() > shown {
                            let items = items.borrow();
                            let mut overflow = 0usize;
                            for index in &chosen[shown..] {
                                if items.get(*index).is_some() {
                                    overflow += 1;
                                }
                            }
                            format_smolstr!("+{overflow}")
                        } else {
                            SmolStr::new_static("")
                        }
                    }
                })
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_overflow.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    row,
                )?;
                overflow.set(Some(overflow_text));
            }
            // 清空按钮（可见性由 `bind_style.visible` 按选中集切换）。
            if clearable {
                let theme_clear = theme.clone();
                let clear = NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .size(size.icon_size(), size.icon_size())
                            .build(),
                    )
                    .a11y(A11yOverride {
                        role: Some(A11yRole::Button),
                        label: Some(SmolStr::new_static("Clear")),
                        ..A11yOverride::default()
                    })
                    .on_click(clear_action.clone())
                    .bind_style(move |cx| StylePatch {
                        visible: Some(!cx.read(&chosen).is_empty()),
                        foreground: Some(theme_clear.role(cx, Role::FgMuted)),
                        ..StylePatch::default()
                    })
                    .build(
                        app.window_mut(window)
                            .ok_or(EngineError::UnknownWindow { window })?
                            .tree_mut(),
                        parent,
                    )?;
                let theme_icon = theme.clone();
                let tree = app
                    .window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut();
                let icon_style = StyleBuilder::new()
                    .size(size.icon_size(), size.icon_size())
                    .build();
                NodeBuilder::text(
                    SmolStr::new_static("×"),
                    TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
                )
                .style(icon_style)
                .a11y(A11yOverride {
                    hidden: true,
                    ..A11yOverride::default()
                })
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_icon.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(tree, clear)?;
            }
            // caret。
            let theme_caret = theme.clone();
            mount_dynamic_icon(
                app,
                window,
                parent,
                IconKind::CaretDown,
                size.icon_size(),
                move |cx| StylePatch {
                    foreground: Some(theme_caret.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                },
            )?;
            Ok(())
        })?;
        {
            let mut state = self.inner.state.borrow_mut();
            state.trigger = Some(node);
            state.slots = slots_sink.borrow().clone();
            state.overflow = overflow_sink.get();
            state.trigger_icon = icon_slot.get();
            state.icon_shown = current_icon;
        }
        self.sync_trigger_a11y(app);
        self.sync_tags(app);
        Ok(node)
    }

    /// 接引擎：触发器上的 `↓` / `↑` 作用域键（必须在 `build` 之后）。
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode`。
    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let Some(trigger) = self.inner.state.borrow().trigger else {
            return Ok(());
        };
        for (key, action) in [
            (KeyCode::ArrowDown, self.inner.actions.down.clone()),
            (KeyCode::ArrowUp, self.inner.actions.up.clone()),
        ] {
            app.bind_key_scoped(
                window,
                trigger,
                KeyBinding {
                    key,
                    modifiers: Modifiers::empty(),
                    action,
                },
            )?;
        }
        Ok(())
    }

    /// 一步完成 `build` + `attach`。
    ///
    /// # Errors
    /// 同 [`Select::build`] / [`Select::attach`]。
    pub fn mount(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let node = self.build(app, window, parent)?;
        self.attach(app, window)?;
        Ok(node)
    }

    /// 打开面板。
    ///
    /// # Errors
    /// 浮层参数 / 引擎建树错误。
    pub fn open(&self, app: &mut App) -> Result<bool, OverlayError> {
        let (Some(window), Some(trigger)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().trigger,
        ) else {
            return Ok(false);
        };
        self.measure_trigger(app);
        self.resolve_icons(app, window)
            .map_err(OverlayError::from)?;
        let width = self
            .inner
            .options
            .width
            .unwrap_or_else(|| self.inner.state.borrow().trigger_width.max(1.0));
        self.inner
            .panel
            .set_content(app, self.panel_content(app))
            .map_err(OverlayError::from)?;
        let opened = self
            .inner
            .panel
            .open(app, window, Anchor::node(trigger), width)?;
        if opened {
            let row = self
                .chosen_indices(app)
                .first()
                .copied()
                .or_else(|| self.first_enabled());
            self.inner.panel.set_highlighted(app, row);
            let _ = self.inner.panel.focus(app);
        }
        Ok(opened)
    }

    /// 关闭面板。
    pub fn close(&self, app: &mut App) -> bool {
        self.inner.panel.close(app)
    }

    /// 立即关闭面板。
    pub fn close_now(&self, app: &mut App) -> bool {
        self.inner.panel.close_now(app)
    }

    /// 帧同步：触发器 / 选中项图标 / 标签槽 / 面板行 / 宽度对齐。
    ///
    /// # Errors
    /// 引擎列表同步错误。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.inner.trigger.sync(app)?;
        changed |= self.sync_trigger_a11y(app);
        changed |= self.sync_trigger_icon(app)?;
        changed |= self.sync_tags(app);
        if self.is_open(app) {
            self.measure_trigger(app);
            if self.inner.options.width.is_none() {
                let width = self.inner.state.borrow().trigger_width.max(1.0);
                self.inner.panel.resize(app, width)?;
            }
        }
        changed |= self.inner.panel.sync(app, true)?;
        Ok(changed)
    }

    /// 转发平台事件（触发器按下态；行点击由引擎动作承担，不在这里补激活）。
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.inner.trigger.dispatch(app, event)
    }

    /// 释放组件（触发器 / 面板 / 动作 handler）。
    pub fn release(&self, app: &mut App) -> usize {
        for action in [
            &self.inner.actions.trigger_click,
            &self.inner.actions.down,
            &self.inner.actions.up,
            &self.inner.actions.clear,
        ] {
            app.off_action(action);
        }
        self.inner.trigger.release(app) + self.inner.panel.release(app)
    }

    // ── 内部 ──

    fn panel_content(&self, app: &App) -> PanelContent {
        let items = self.inner.items.borrow().clone();
        let chosen_snapshot = self.chosen_indices(app);
        let rows: Vec<PanelRow> = items
            .iter()
            .map(|item| PanelRow {
                height: self.inner.options.size.h(),
                disabled: item.disabled,
            })
            .collect();
        let theme = self.inner.theme.clone();
        let size = self.inner.options.size;
        let chosen = self.inner.chosen;
        let labels: Vec<SmolStr> = items.iter().map(|item| item.label.clone()).collect();
        let icons = Rc::new(self.inner.icons.snapshot());
        let items_build = items.clone();
        let has_icon = items.iter().any(|item| item.icon.is_some());
        let build: Rc<dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>> = {
            let labels = labels.clone();
            Rc::new(move |tree, parent, index| {
                let Some(label) = labels.get(index) else {
                    return Ok(());
                };
                let icon = items_build.get(index).and_then(|item| item.icon);
                build_icon_slot(tree, parent, &theme, &icons, icon, size, has_icon)?;
                let theme_text = theme.clone();
                NodeBuilder::text(
                    label.clone(),
                    TextStyle::new(size.text_size())
                        .with_wrap(TextWrap::NoWrap)
                        .with_overflow(TextOverflow::Ellipsis, 1),
                )
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_text.role(cx, Role::FgDefault)),
                    ..StylePatch::default()
                })
                .build(tree, parent)?;
                let theme_check = theme.clone();
                NodeBuilder::text(
                    SmolStr::new_static("✓"),
                    TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
                )
                .style(StyleBuilder::new().width(CHECK_COLUMN).flex_none().build())
                .bind_style(move |cx| StylePatch {
                    visible: Some(cx.read(&chosen).contains(&index)),
                    foreground: Some(theme_check.role(cx, Role::FgDefault)),
                    ..StylePatch::default()
                })
                .build(tree, parent)
                .map(|_| ())
            })
        };
        let a11y_labels = labels.clone();
        let a11y_chosen = chosen_snapshot;
        let disabled: Vec<bool> = items.iter().map(|item| item.disabled).collect();
        let a11y: Rc<dyn Fn(usize) -> A11yPatch> =
            Rc::new(move |index| match a11y_labels.get(index) {
                Some(label) => A11yPatch {
                    role: Some(A11yRole::ListBoxOption),
                    label: Some(label.clone()),
                    disabled: disabled.get(index).copied(),
                    selected: Some(a11y_chosen.contains(&index)),
                    checked: Some(a11y_chosen.contains(&index)),
                    ..A11yPatch::default()
                },
                None => A11yPatch {
                    hidden: Some(true),
                    ..A11yPatch::default()
                },
            });
        let extra = CHECK_COLUMN + if has_icon { size.icon_size() } else { 0.0 };
        let measure = Some(PanelMeasure { labels, extra });
        PanelContent {
            rows,
            build,
            a11y,
            measure,
        }
    }

    fn install_handlers(&self, app: &mut App) {
        let this = self.clone();
        let disabled = self.inner.trigger.disabled_signal();
        let click = self.inner.actions.trigger_click.clone();
        app.on_action(click, move |ctx: &mut ActionCtx<'_>| {
            if *ctx.read(&disabled) {
                return;
            }
            let app = ctx.app_mut();
            if this.is_open(app) {
                this.close(app);
            } else {
                let _ = this.open(app);
            }
        });
        let this = self.clone();
        app.on_action(self.inner.actions.down.clone(), move |ctx| {
            let app = ctx.app_mut();
            let _ = this.open(app);
            let row = this
                .chosen_indices(app)
                .first()
                .copied()
                .or_else(|| this.first_enabled());
            this.inner.panel.set_highlighted(app, row);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.up.clone(), move |ctx| {
            let app = ctx.app_mut();
            let _ = this.open(app);
            let items = this.inner.items.borrow();
            let last = (0..items.len()).rev().find(|index| !items[*index].disabled);
            drop(items);
            this.inner.panel.set_highlighted(app, last);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.clear.clone(), move |ctx| {
            let app = ctx.app_mut();
            if this.clear(app) {
                let _ = this.inner.panel.close(app);
            }
        });
        let this = self.clone();
        self.inner.panel.on_activate(move |app, row| {
            this.activate_row(app, row);
        });
    }

    /// 面板行激活：单选取值并关面板；多选切换并保持打开。
    fn activate_row(&self, app: &mut App, row: usize) {
        let item = self.inner.items.borrow().get(row).cloned();
        let Some(item) = item else {
            return;
        };
        if item.disabled {
            return;
        }
        let multiple = self.inner.options.multiple;
        let mut chosen = self.chosen_indices(app);
        if multiple {
            if let Some(position) = chosen.iter().position(|index| *index == row) {
                chosen.remove(position);
            } else {
                chosen.push(row);
            }
            chosen.sort_unstable();
            app.write(&self.inner.chosen, chosen);
        } else {
            // 重复选中同一项不回调（旧 `select`：`id.ne(selected_id)` 过滤），但仍关闭面板。
            let unchanged = chosen.as_slice() == [row];
            app.write(&self.inner.chosen, vec![row]);
            self.sync_trigger_a11y(app);
            let _ = self.close(app);
            if !unchanged {
                self.notify(app);
            }
            return;
        }
        let _ = self.sync_tags(app);
        self.sync_trigger_a11y(app);
        self.notify(app);
    }

    /// 触发器可访问名：显式 `label` 优先；否则 = 选中项文本（多选 = 首项 + `+N` 摘要）/ 占位文本。
    fn sync_trigger_a11y(&self, app: &mut App) -> bool {
        if self.inner.options.label.is_some() {
            return false;
        }
        let next = {
            let indices = self.chosen_indices(app);
            let items = self.inner.items.borrow();
            match indices.split_first() {
                Some((first, rest)) => {
                    let label = items
                        .get(*first)
                        .map(|item| item.label.clone())
                        .unwrap_or_default();
                    if rest.is_empty() {
                        label
                    } else {
                        format_smolstr!("{label} +{}", rest.len())
                    }
                }
                None => self.inner.options.placeholder.clone(),
            }
        };
        app.write(&self.inner.a11y_label, next)
    }

    /// 当前选中集的第一个图标（单选 = 选中项；多选 = 首项）。
    fn selected_icon(&self, app: &App) -> Option<IconKind> {
        let indices = self.chosen_indices(app);
        let first = *indices.first()?;
        self.inner
            .items
            .borrow()
            .get(first)
            .and_then(|item| item.icon)
    }

    /// 解析条目图标资源（行工厂拿不到 `App`，行内容建树读快照）。
    fn resolve_icons(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let size = self.inner.options.size.icon_size();
        let items = self.inner.items.borrow().clone();
        let mut kinds: Vec<IconKind> = items.iter().filter_map(|item| item.icon).collect();
        kinds.extend(self.selected_icon(app));
        kinds.sort_unstable();
        kinds.dedup();
        for kind in kinds {
            self.inner.icons.resolve(app, window, kind, size)?;
        }
        Ok(())
    }

    /// 选中集变化 ⇒ 换触发器前导图标的资源与占位尺寸。
    fn sync_trigger_icon(&self, app: &mut App) -> Result<bool, EngineError> {
        let (Some(window), Some(node)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().trigger_icon,
        ) else {
            return Ok(false);
        };
        let desired = self.selected_icon(app);
        {
            let state = self.inner.state.borrow();
            if state.icon_shown == desired {
                return Ok(false);
            }
        }
        let size = self.inner.options.size.icon_size();
        let mut changed = false;
        if let Some(kind) = desired {
            self.inner.icons.resolve(app, window, kind, size)?;
            changed = true;
        }
        let resource = desired.and_then(|kind| self.inner.icons.snapshot().get(kind, size));
        if let Some(target) = app.window_mut(window) {
            if let Ok(current) = target.tree().get(node) {
                let mut style = current.style().clone();
                let side = if desired.is_some() { size } else { 0.0 };
                style.inner.size = taffy::geometry::Size {
                    width: Dimension::length(side),
                    height: Dimension::length(side),
                };
                let _ = target.tree_mut().set_style(node, style);
            }
            if let Some(resource) = resource {
                let _ = target.set_image_resource(node, resource);
            }
        }
        self.inner.state.borrow_mut().icon_shown = desired;
        Ok(changed)
    }

    /// 选中集变化回调。
    fn notify(&self, app: &mut App) {
        if let Some(callback) = self.inner.on_change.borrow().clone() {
            let chosen = self.chosen(app);
            callback(app, chosen);
        }
    }

    /// 标签槽显示切换（选中集变化时写 `display`，其余帧不写 —— I3）。
    fn sync_tags(&self, app: &mut App) -> bool {
        let multiple = self.inner.options.multiple;
        if !multiple {
            return false;
        }
        let chosen = self.chosen_indices(app);
        let mut state = self.inner.state.borrow_mut();
        if state.tags_written == chosen {
            return false;
        }
        let Some(window) = state.window else {
            return false;
        };
        let max_tags = self.inner.options.max_tags.max(1);
        let slots = state.slots.clone();
        let overflow = state.overflow;
        state.tags_written = chosen.clone();
        drop(state);
        let Some(tree) = app.window_mut(window).map(|target| target.tree_mut()) else {
            return false;
        };
        for (slot, node) in slots.iter().enumerate() {
            let Some(node) = node else {
                continue;
            };
            let display = if slot < chosen.len().min(max_tags) {
                Display::Flex
            } else {
                Display::None
            };
            set_display(tree, *node, display);
        }
        if let Some(overflow) = overflow {
            set_display(
                tree,
                overflow,
                if chosen.len() > max_tags {
                    Display::Flex
                } else {
                    Display::None
                },
            );
        }
        true
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.inner
            .items
            .borrow()
            .iter()
            .position(|item| item.id == id)
    }

    fn first_enabled(&self) -> Option<usize> {
        self.inner
            .items
            .borrow()
            .iter()
            .position(|item| !item.disabled)
    }

    fn measure_trigger(&self, app: &App) {
        let (Some(window), Some(trigger)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().trigger,
        ) else {
            return;
        };
        let Some(width) = app
            .window(window)
            .and_then(|target| target.tree().get(trigger).ok())
            .map(|node| node.layout().bounds.size.width)
        else {
            return;
        };
        self.inner.state.borrow_mut().trigger_width = width;
    }
}

/// 写一个节点的 `display`（标签槽显示切换；布局脏由 `set_style` 负责）。
fn set_display(tree: &mut NodeTree, node: NodeId, display: Display) {
    if let Ok(target) = tree.get(node) {
        let mut style = target.style().clone();
        if style.inner.display == display {
            return;
        }
        style.inner.display = display;
        let _ = tree.set_style(node, style);
    }
}

/// 标签槽参数（一次建槽需要的共享状态）。
struct TagChip<'a> {
    parent: NodeId,
    size: UnifiedSize,
    theme: &'a ThemeHandle,
    chosen: Signal<Vec<usize>>,
    revision: Signal<u64>,
    items: &'a Rc<RefCell<Rc<Vec<SelectItem>>>>,
    key: &'a str,
}

/// 一个标签槽（`Badge` 中性色块的等价视觉 + `bind_text` 动态文本）。
fn build_tag_chip(
    app: &mut App,
    window: WindowId,
    chip: TagChip<'_>,
    slot: usize,
) -> Result<NodeId, EngineError> {
    let TagChip {
        parent,
        size,
        theme,
        chosen,
        revision,
        items,
        key,
    } = chip;
    let chip_height = (size.h() - 8.0).max(16.0);
    let theme_chip = theme.clone();
    let tree = app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut();
    let chip = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:tag:{slot}"))
        .style(hidden_style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .height(chip_height)
                .padding_x(size.px() / 2.0)
                .border(1.0)
                .build(),
        ))
        .paint(PaintBuilder::new().rounded(TAG_RADIUS).build())
        .bind_style(move |cx| StylePatch {
            background: Some(theme_chip.role(cx, Role::SurfaceControl)),
            foreground: Some(theme_chip.role(cx, Role::FgDefault)),
            border: Some(Border {
                width: 1.0,
                color: theme_chip.role(cx, Role::LineBorder),
            }),
            ..StylePatch::default()
        })
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)?;
    let theme_text = theme.clone();
    let items = items.clone();
    let text = NodeBuilder::text(
        SmolStr::new_static(""),
        TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
    )
    .bind_text(move |cx| {
        let _ = *cx.read(&revision);
        let chosen = cx.read(&chosen);
        let items = items.borrow();
        chosen
            .get(slot)
            .and_then(|index| items.get(*index))
            .map(|item| item.label.clone())
            .unwrap_or_default()
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(theme_text.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(
        app.window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut(),
        chip,
    )?;
    let _ = text;
    Ok(chip)
}

/// 初始隐藏的样式（标签槽 / `+N` 先不占位，`sync` 按选中集写 `display`）。
fn hidden_style(mut style: ui_core::paint::style::Style) -> ui_core::paint::style::Style {
    style.inner.display = Display::None;
    style
}

impl fmt::Debug for Select {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Select")
            .field("key", &self.inner.key)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    //! 引擎树上的行为：单选 / 多选 / 标签 / 清空 / 禁用 / 键盘 / 关闭与焦点 / a11y / I3。

    use std::{cell::RefCell, rc::Rc, time::Duration};

    use smol_str::SmolStr;
    use ui_core::{
        a11y::A11yRole,
        event::{Action, Event, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Point, Rect},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{Select, SelectItem, SelectOptions};
    use crate::{
        kit::{
            icon::IconKind,
            layer::Layer,
            overlay::AnimationRouter,
            test_support::{a11y_snapshot_without_oracle, overlay_wrapper},
        },
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        changed: Rc<RefCell<Vec<Vec<SmolStr>>>>,
    }

    impl Harness {
        fn new() -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            NodeBuilder::boxed()
                .id("background")
                .layout_override(Rect::new(620.0, 520.0, 80.0, 40.0))
                .on_click(Action::Custom(SmolStr::new_static("background.click")))
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("背景节点");
            Self {
                app,
                window,
                theme,
                layer: Layer::new(),
                router: AnimationRouter::new(),
                changed: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn select(&mut self, options: SelectOptions) -> Select {
            let changed = self.changed.clone();
            Select::new(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                self.router.clone(),
                "select",
                options,
            )
            .on_change(move |_, chosen| changed.borrow_mut().push(chosen))
        }

        fn mount(&mut self, select: &Select, items: impl IntoIterator<Item = SelectItem>) {
            let root = self
                .app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .root();
            select.set_items(self.app.app_mut(), items).expect("条目");
            select
                .mount(self.app.app_mut(), self.window, root)
                .expect("挂载");
            self.settle(select);
        }

        fn settle(&mut self, select: &Select) {
            for _ in 0..6 {
                self.app.advance(Duration::from_millis(40));
                let _ = self.app.drain().expect("帧");
                let _ = select.sync(self.app.app_mut());
            }
            self.app.advance(Duration::from_millis(200));
            let _ = self.app.drain().expect("帧");
            let _ = select.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
        }

        fn focus_trigger(&mut self, select: &Select) {
            let trigger = select.trigger_node().expect("触发器");
            self.app.app_mut().set_focus(self.window, Some(trigger));
        }

        fn key(&mut self, select: &Select, code: KeyCode) {
            let _ = self
                .app
                .inject(InputEvent::key_down(code, Modifiers::empty()));
            let _ = self
                .app
                .inject(InputEvent::key_up(code, Modifiers::empty()));
            self.settle(select);
        }

        fn click(&mut self, select: &Select, at: Point) {
            let _ = self.app.inject(InputEvent::pointer_down(
                at,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.inject(InputEvent::pointer_up(
                at,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            self.settle(select);
        }

        fn bounds(&self, node: NodeId) -> Rect {
            self.app.probe().bounds(node).expect("布局盒")
        }

        fn center(&self, node: NodeId) -> Point {
            let bounds = self.bounds(node);
            Point::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        fn find(&self, debug_id: &str) -> Option<NodeId> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![tree.root()];
            while let Some(node) = stack.pop() {
                if tree.debug_id(node) == Some(debug_id) {
                    return Some(node);
                }
                if let Ok(target) = tree.get(node) {
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            None
        }

        fn row(&self, index: usize) -> NodeId {
            self.find(&format!("select:panel:list:row:{index}"))
                .unwrap_or_else(|| panic!("第 {index} 行未挂载"))
        }

        /// 子树里**有尺寸**的图片节点数（尺寸 0 的图标槽 = 未显示）。
        fn visible_images(&self, node: NodeId) -> usize {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![node];
            let mut count = 0;
            while let Some(current) = stack.pop() {
                if let Ok(target) = tree.get(current) {
                    if matches!(target.kind(), NodeKind::Image(_))
                        && target
                            .style()
                            .inner
                            .size
                            .width
                            .into_option()
                            .is_some_and(|width| width > 0.0)
                    {
                        count += 1;
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            count
        }

        fn row_center(&self, index: usize) -> Point {
            self.center(self.row(index))
        }

        fn text_of(&self, node: NodeId) -> Option<SmolStr> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![node];
            while let Some(current) = stack.pop() {
                if let Ok(target) = tree.get(current) {
                    if let NodeKind::Text(content) = target.kind() {
                        return Some(content.text.clone());
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            None
        }

        fn trigger_text(&self, select: &Select) -> SmolStr {
            self.text_of(select.trigger_node().expect("触发器"))
                .expect("触发器里没有文本节点")
        }

        fn display_of(&self, node: NodeId) -> taffy::style::Display {
            self.app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .get(node)
                .expect("节点")
                .style()
                .inner
                .display
        }

        fn idles(&mut self) -> bool {
            !self.app.drain().expect("帧").rendered
        }
    }

    fn items() -> Vec<SelectItem> {
        vec![
            SelectItem::new("a", "Alpha"),
            SelectItem::new("b", "Beta"),
            SelectItem::new("c", "Gamma").disabled(true),
        ]
    }

    #[test]
    fn a_single_select_chooses_one_item_and_shows_it_on_the_trigger() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().placeholder("请选择"));
        harness.mount(&select, items());
        assert_eq!(harness.trigger_text(&select), "请选择", "空选中显示占位");

        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()));
        let at = harness.row_center(1);
        harness.click(&select, at);

        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")],
            "单选写入选中集"
        );
        assert_eq!(harness.changed.borrow().len(), 1, "选中变化回调一次");
        assert!(!select.is_open(harness.app.app()), "单选选中即关");
        assert_eq!(harness.trigger_text(&select), "Beta", "触发器显示选中项");

        // 再点触发器打开，再点一次关闭（旧 `pressing_the_trigger_of_an_open_select_should_close_it`）。
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "点击触发器打开");
        harness.click(&select, harness.center(trigger));
        assert!(!select.is_open(harness.app.app()), "再点触发器关闭");
    }

    #[test]
    fn a_multi_select_toggles_items_and_keeps_the_panel_open() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().multiple(true).max_tags(2));
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));

        let first = harness.row_center(0);
        harness.click(&select, first);
        assert!(select.is_open(harness.app.app()), "多选切换后面板保持打开");
        let second = harness.row_center(1);
        harness.click(&select, second);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("a"), SmolStr::new_static("b")],
            "多选累计选中"
        );
        assert_eq!(harness.changed.borrow().len(), 2);

        // 标签槽：前两个显示，第三个（不存在）隐藏。
        let tag0 = harness.find("select:tag:0").expect("标签 0");
        let tag1 = harness.find("select:tag:1").expect("标签 1");
        assert_eq!(harness.display_of(tag0), taffy::style::Display::Flex);
        assert_eq!(harness.display_of(tag1), taffy::style::Display::Flex);
        assert_eq!(harness.text_of(tag0), Some(SmolStr::new_static("Alpha")));
        assert_eq!(harness.text_of(tag1), Some(SmolStr::new_static("Beta")));

        // 再点一次取消。
        let first = harness.row_center(0);
        harness.click(&select, first);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")],
            "再点切换回未选中"
        );
        assert_eq!(harness.display_of(tag1), taffy::style::Display::None);
    }

    #[test]
    fn a_multi_select_shows_an_overflow_counter_beyond_the_tag_limit() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().multiple(true).max_tags(1));
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let first = harness.row_center(0);
        harness.click(&select, first);
        let second = harness.row_center(1);
        harness.click(&select, second);

        let texts: Vec<SmolStr> = {
            let tree = harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree();
            let mut stack = vec![tree.root()];
            let mut found = Vec::new();
            while let Some(node) = stack.pop() {
                if let Ok(target) = tree.get(node) {
                    if let NodeKind::Text(content) = target.kind() {
                        found.push(content.text.clone());
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            found
        };
        assert!(
            texts.iter().any(|text| text == "+1"),
            "超出标签上限显示 +N：{texts:?}"
        );
    }

    #[test]
    fn a_disabled_item_cannot_be_chosen() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let at = harness.row_center(2);
        harness.click(&select, at);
        assert!(select.chosen(harness.app.app()).is_empty(), "禁用项不可选");
    }

    #[test]
    fn clearing_the_selection_empties_it_and_reports_once() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().clearable(true));
        harness.mount(&select, items());
        select.set_chosen(harness.app.app_mut(), ["b"], false);
        harness.settle(&select);
        assert_eq!(select.chosen(harness.app.app()).len(), 1);

        assert!(select.clear(harness.app.app_mut()), "清空成功");
        harness.settle(&select);
        assert!(select.chosen(harness.app.app()).is_empty());
        assert_eq!(harness.changed.borrow().len(), 1, "清空回调一次");
    }

    #[test]
    fn arrow_down_opens_the_panel_and_enter_chooses_the_highlighted_item() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        harness.focus_trigger(&select);
        harness.key(&select, KeyCode::ArrowDown);
        assert!(select.is_open(harness.app.app()), "↓ 打开面板");
        assert_eq!(select.highlighted(harness.app.app()), Some(0));
        harness.key(&select, KeyCode::End);
        assert_eq!(
            select.highlighted(harness.app.app()),
            Some(1),
            "End 到末个可选项"
        );
        harness.key(&select, KeyCode::Enter);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")]
        );
    }

    #[test]
    fn escape_and_outside_presses_close_the_panel_and_return_focus() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");

        harness.click(&select, harness.center(trigger));
        harness.key(&select, KeyCode::Escape);
        assert!(!select.is_open(harness.app.app()), "Esc 关闭");
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger),
            "关闭后焦点回到触发器"
        );

        harness.click(&select, harness.center(trigger));
        harness.click(&select, Point::new(10.0, 10.0));
        assert!(!select.is_open(harness.app.app()), "点击外部关闭");
    }

    #[test]
    fn set_items_keeps_the_chosen_items_by_id() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        select.set_chosen(harness.app.app_mut(), ["b"], false);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")]
        );

        select
            .set_items(
                harness.app.app_mut(),
                vec![SelectItem::new("z", "Zeta"), SelectItem::new("b", "Beta 2")],
            )
            .expect("换条目");
        harness.settle(&select);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")],
            "按 id 保留选中"
        );
        assert_eq!(harness.trigger_text(&select), "Beta 2", "文本随条目更新");
    }

    #[test]
    fn the_panel_declares_list_and_list_box_option_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let panel = select.panel_node().expect("面板");
        let wrapper = overlay_wrapper(harness.app.app(), harness.window, panel);
        assert_eq!(
            snapshot.node_for(wrapper).expect("包装").role,
            A11yRole::GenericContainer,
            "ListBox 角色缺位（E15）时包装不声明角色"
        );
        assert_eq!(
            snapshot
                .nodes()
                .iter()
                .filter(|node| node.role == A11yRole::List)
                .count(),
            1,
            "集合语义只来自内层 E9 虚拟列表容器（不得双 List）"
        );
        let options: Vec<String> = snapshot
            .nodes()
            .iter()
            .filter(|node| node.role == A11yRole::ListBoxOption)
            .filter_map(|node| node.label.as_ref().map(|label| label.text().to_owned()))
            .collect();
        assert!(
            options.contains(&"Alpha".to_owned()),
            "选项角色：{options:?}"
        );
    }

    #[test]
    fn a_settled_select_with_no_input_requests_no_frames() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        assert!(!select.sync(harness.app.app_mut()).expect("同步"));
        assert!(harness.idles(), "稳态不出帧（I3）");
    }

    #[test]
    fn a_forwarded_pointer_up_does_not_toggle_the_row_twice() {
        // 审查 P1-2：外壳在 `drain_events` 之后按约定把每条事件转发给 `dispatch`；行点击
        // 已由引擎动作完成，`dispatch` 不得再激活一次（多选面板会「点一次切两次」净效果 0）。
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions {
            multiple: true,
            ..SelectOptions::default()
        });
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let at = harness.row_center(0);
        harness.click(&select, at);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("a")],
            "一次点击只切换一次"
        );

        let forwarded = Event::Input(InputEvent::pointer_up(
            at,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert!(
            !select.dispatch(harness.app.app_mut(), &forwarded),
            "dispatch 不再兜底行点击"
        );
        harness.settle(&select);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("a")],
            "转发同一 PointerUp 后不重复切换"
        );
    }

    #[test]
    fn choosing_the_same_single_item_again_does_not_notify() {
        // 旧 `select` 的 `id.ne(selected_id)` 过滤：重复选中同一项不回调，但面板照常关闭。
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");

        harness.click(&select, harness.center(trigger));
        harness.click(&select, harness.row_center(1));
        assert_eq!(harness.changed.borrow().len(), 1, "首次选中回调一次");

        harness.click(&select, harness.center(trigger));
        harness.click(&select, harness.row_center(1));
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")]
        );
        assert_eq!(harness.changed.borrow().len(), 1, "重复选中同一项不再回调");
        assert!(!select.is_open(harness.app.app()), "单选选中即关");
    }

    #[test]
    fn a_long_item_grows_the_panel_past_its_trigger() {
        // 旧 `min_w(trigger)` 是下限：内容更宽时面板长到内容宽（不是精确等宽）。
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(
            &select,
            vec![
                SelectItem::new(
                    "long",
                    "A very long selection entry that cannot fit the trigger width at all",
                ),
                SelectItem::new("short", "B"),
            ],
        );
        let trigger = select.trigger_node().expect("触发器");
        let trigger_width = harness.bounds(trigger).size.width;
        harness.click(&select, harness.center(trigger));
        let panel = harness.bounds(select.panel_node().expect("面板"));
        assert!(
            panel.size.width > trigger_width + 100.0,
            "长条目把面板撑宽（旧 min_w 下限口径）：面板 {} / 触发器 {trigger_width}",
            panel.size.width
        );
    }

    #[test]
    fn item_icons_are_drawn_in_the_rows_and_on_the_trigger() {
        // 旧 `Select` 的条目是 `MenuItem`（可带 `IconAction`），触发器画选中项图标。
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(
            &select,
            vec![
                SelectItem::new("a", "Alpha").icon(IconKind::Circle),
                SelectItem::new("b", "Beta"),
            ],
        );
        let trigger = select.trigger_node().expect("触发器");
        assert_eq!(
            harness.visible_images(trigger),
            1,
            "没有选中项时触发器只有 caret"
        );

        harness.click(&select, harness.center(trigger));
        let panel = select.panel_node().expect("面板");
        assert_eq!(
            harness.visible_images(panel),
            1,
            "只有带图标的条目画图标（其余行留占位槽）"
        );

        harness.click(&select, harness.row_center(0));
        harness.settle(&select);
        assert_eq!(
            harness.visible_images(trigger),
            2,
            "选中带图标的条目后触发器画该图标 + caret"
        );
    }

    #[test]
    fn the_trigger_reports_the_selection_and_the_expanded_state() {
        // 审查 F8：触发器 a11y 可访问名跟选中项走、展开态走 `aria-expanded`。
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");

        harness.settle(&select);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(record.label_text(), Some("Select…"), "占位文本是可访问名");
        assert_eq!(record.expanded, Some(false), "关闭态 aria-expanded=false");

        harness.click(&select, harness.center(trigger));
        harness.settle(&select);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.expanded, Some(true), "打开态 aria-expanded=true");

        harness.click(&select, harness.row_center(1));
        harness.settle(&select);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.label_text(), Some("Beta"), "可访问名跟随选中项");
        assert_eq!(record.expanded, Some(false), "关闭后 expanded 复原");
    }

    #[test]
    fn a_multi_select_trigger_reports_a_selection_summary() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let select = harness.select(SelectOptions {
            multiple: true,
            ..SelectOptions::default()
        });
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        harness.click(&select, harness.row_center(0));
        harness.click(&select, harness.row_center(1));
        harness.settle(&select);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(
            record.label_text(),
            Some("Alpha +1"),
            "多选触发器给出选中摘要（旧实现完全不可读）"
        );
    }
}

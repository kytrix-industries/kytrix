//! 下拉（P2-4c：旧 `crate::dropdown` 的引擎化重建）。
//!
//! 旧 `Dropdown` =「按钮式触发器（`Interactive`）+ 挂 `Menu` 的 `Overlay`」：触发器点击写 keyed 的
//! open 状态，`Overlay::on_prepaint` 量触发器宽度给面板 `min_w`，`on_mouse_down_out` 关面板。
//! 新引擎里这四件事分别落在：触发器 [`Interactive`]（hover / 按下 / 焦点 / 禁用 + 点击动作）、
//! 共享条目面板 [`ItemPanel`]（浮层定位 / 点击外部关闭 / Esc / 焦点陷阱 / 虚拟化行）、
//! `bind_text`（触发器文本）、`bind_style`（占位弱化）。
//!
//! # 行为
//!
//! - 点击触发器打开 / 再点关闭；
//! - `↓` 在触发器上打开面板并把高亮落到选中项（没有选中则首行），`↑` 打开并落到末行；
//! - 面板内 `↑` / `↓` / `Home` / `End` 移动高亮，`Enter` / `Space` 选中高亮项；
//! - 选中项写回触发器文本并回调 `on_select`，然后关闭面板（旧实现同款：选中即关）；
//! - **不自动选中首项**：没有选中项时触发器显示占位文本（旧 dropdown 没有选中状态，首项回退
//!   只属于 select 的「打开时高亮」）；
//! - 面板宽度默认对齐触发器实测宽度，但它只是**下限**：内容更宽时面板长到内容宽（旧
//!   `min_w(trigger)` 口径）；[`DropdownOptions::width`] 显式给宽时按面板口径精确使用；
//!   打开期间触发器宽度变化会在 `sync` 里重新对齐；
//! - 面板高度上限 [`DropdownOptions::max_height`]，超出滚动（旧 `menu_max_h`）；
//! - 条目支持图标（旧 dropdown 的条目就是 `MenuItem`，可带 `IconAction`）：有图标的条目整列
//!   留槽；选中项的图标同时画在触发器上（换选中时 `sync` 换资源与占位尺寸）。
//!
//! # 与旧实现的差异（已接受，见 spec-delta）
//!
//! - 触发器内容由组件自建（文本 + 可选图标 + caret），不再接受调用方任意 children；
//! - 面板是**模态浮层**：打开态外部按下与背景滚动被消费（旧实现外部按下不消费）。
//!
//! # a11y
//!
//! 触发器是 `Button`（引擎没有 `ComboBox` 角色，登记见 spec-delta），面板 `Menu` + 行 `MenuItem`
//! 由 [`ItemPanel`] 声明；没有显式 `label` 时触发器的可访问名跟随选中项 / 占位文本，
//! 编展开态写进 `aria-expanded`。

use std::{cell::RefCell, fmt, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::Dimension;
use ui_core::{
    a11y::{A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, KeyBinding, KeyCode, Modifiers},
    interaction::ActionCtx,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::style::StylePatch,
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
    style_builder::StyleBuilder,
    theme::{components::InteractiveVariant, handle::ThemeHandle, role::Role},
};

/// 下拉条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownItem {
    /// 条目 id（回调里报告给调用方）。
    pub id: SmolStr,
    /// 显示文本（同时是可访问名）。
    pub label: SmolStr,
    /// 条目图标（旧 dropdown 的条目就是 `MenuItem`，支持 `IconAction`）；有图标时整列留槽。
    pub icon: Option<IconKind>,
    /// 禁用（点击不选中）。
    pub disabled: bool,
}

impl DropdownItem {
    /// 条目。
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

/// 下拉参数。
#[derive(Clone, Debug, PartialEq)]
pub struct DropdownOptions {
    /// 尺寸档（触发器高度 / 字号 / 行高）。
    pub size: UnifiedSize,
    /// 触发器视觉变体。
    pub variant: InteractiveVariant,
    /// 面板宽度（`None` = 对齐触发器实测宽度）。
    pub width: Option<f32>,
    /// 面板最大高度（逻辑像素）。
    pub max_height: f32,
    /// 面板相对触发器的定位。
    pub placement: Placement,
    /// 与触发器的间隙。
    pub gap: f32,
    /// 触发器可访问名（`None` 用占位文本）。
    pub label: Option<SmolStr>,
    /// 没有选中项时的占位文本。
    pub placeholder: SmolStr,
}

impl Default for DropdownOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            variant: InteractiveVariant::Ghost,
            width: None,
            max_height: 320.0,
            placement: Placement::BOTTOM_START,
            gap: 4.0,
            label: None,
            placeholder: SmolStr::new_static("Select…"),
        }
    }
}

impl DropdownOptions {
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

    /// 面板宽度（`None` = 对齐触发器）。
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
}

/// 下拉（`Rc` 浅拷贝）。
#[derive(Clone)]
pub struct Dropdown {
    inner: Rc<DropdownInner>,
}

#[derive(Default)]
struct DropdownState {
    window: Option<WindowId>,
    trigger: Option<NodeId>,
    /// 触发器宽度（对齐面板用；打开期间变化重对齐）。
    trigger_width: f32,
    /// 触发器前导图标节点（有选中项且该项有图标时占位并绘制）。
    trigger_icon: Option<NodeId>,
    /// 当前图标节点上的图标（`None` = 虚线占位，尺寸 0）。
    icon_shown: Option<IconKind>,
}

struct DropdownActions {
    /// 触发器自身的点击动作。
    trigger_click: Action,
    /// 触发器上的 `↓` / `↑`（打开并定位高亮）。
    down: Action,
    up: Action,
}

struct DropdownInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: DropdownOptions,
    trigger: Interactive,
    panel: ItemPanel,
    icons: IconCache,
    items: RefCell<Rc<Vec<DropdownItem>>>,
    /// 选中条目下标。
    selected: Signal<Option<usize>>,
    /// 触发器显示文本（选中项的 label；空 = 占位）。
    display: Signal<SmolStr>,
    /// 触发器可访问名（选中项文本 / 显式 label / 占位文本；`Interactive` 的 a11y 读它）。
    a11y_label: Signal<SmolStr>,
    on_select: RefCell<Option<Rc<dyn Fn(&mut App, SmolStr)>>>,
    actions: DropdownActions,
    state: RefCell<DropdownState>,
}

impl Dropdown {
    /// 建下拉（关闭态）。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
        options: DropdownOptions,
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
                    panel: Some(A11yRole::Menu),
                    row: A11yRole::MenuItem,
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
        let inner = Rc::new(DropdownInner {
            actions: DropdownActions {
                trigger_click,
                down: Action::Custom(format_smolstr!("{key}.open-down")),
                up: Action::Custom(format_smolstr!("{key}.open-up")),
            },
            key,
            theme,
            options,
            trigger,
            panel,
            icons: IconCache::default(),
            items: RefCell::new(Rc::new(Vec::new())),
            selected: app.signal_scoped(scope, None),
            display: app.signal_scoped(scope, SmolStr::new_static("")),
            a11y_label,
            on_select: RefCell::new(None),
            state: RefCell::new(DropdownState::default()),
        });
        let dropdown = Self { inner };
        dropdown.install_handlers(app);
        dropdown
    }

    /// 下拉键（调试 id / 动作前缀）。
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

    /// 选中条目 id。
    #[must_use]
    pub fn selected(&self, app: &App) -> Option<SmolStr> {
        let index = app.read_with(&self.inner.selected, |selected| *selected)?;
        self.inner
            .items
            .borrow()
            .get(index)
            .map(|item| item.id.clone())
    }

    /// 高亮行（面板打开时）。
    #[must_use]
    pub fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.panel.highlighted(app)
    }

    /// 条目快照。
    #[must_use]
    pub fn items(&self) -> Rc<Vec<DropdownItem>> {
        self.inner.items.borrow().clone()
    }

    /// 换一批条目（选中项按 id 保留，找不到则**清空** —— 不回退首项：没有选中的下拉显示
    /// 占位文本，旧实现同样没有默认选中）。
    ///
    /// # Errors
    /// 引擎虚拟列表失效。
    pub fn set_items(
        &self,
        app: &mut App,
        items: impl IntoIterator<Item = DropdownItem>,
    ) -> Result<(), EngineError> {
        let previous = self.selected(app);
        *self.inner.items.borrow_mut() = Rc::new(items.into_iter().collect());
        let next = previous.as_ref().and_then(|id| self.index_of(id));
        self.apply_selected(app, next);
        if let Some(window) = self.inner.state.borrow().window {
            self.resolve_icons(app, window)?;
        }
        self.inner.panel.set_content(app, self.panel_content())
    }

    /// 选中条目 id（`None` = 清空；找不到的 id 忽略）。
    pub fn set_selected(&self, app: &mut App, id: Option<&str>) {
        let next = id.and_then(|id| self.index_of(id));
        self.apply_selected(app, next);
    }

    /// 选中变化回调（用户选择；程序化 `set_selected` 不触发）。
    pub fn on_select(self, callback: impl Fn(&mut App, SmolStr) + 'static) -> Self {
        *self.inner.on_select.borrow_mut() = Some(Rc::new(callback));
        self
    }

    /// 禁用 / 启用触发器。
    pub fn set_disabled(&self, app: &mut App, disabled: bool) -> bool {
        self.inner.trigger.set_disabled(app, disabled)
    }

    /// 建触发器节点（前导图标 + 文本 + 下拉箭头）。
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
        let display = self.inner.display;
        let selected = self.inner.selected;
        let theme = self.inner.theme.clone();
        let placeholder = self.inner.options.placeholder.clone();
        let trigger = self.inner.trigger.clone();
        let trigger_text = trigger.clone();
        let trigger_text_move = trigger_text.clone();
        let trigger_caret = trigger.clone();
        // 触发器前导图标：当前选中项有图标时显示（旧 select 触发器画选中项图标；下拉同口径）。
        // 先按占位图标建节点（尺寸 0），`sync` 按选中项换资源 / 尺寸。
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
        let icon_slot = Rc::new(std::cell::Cell::new(None));
        let icon_slot_inner = icon_slot.clone();
        let theme_icon = theme.clone();
        let node = trigger.build(app, window, parent, move |app, window, parent| {
            let side = if current_icon.is_some() {
                size.icon_size()
            } else {
                0.0
            };
            let icon = NodeBuilder::image(icon_resource, ui_core::node::image::ImageFit::Fill)
                .style(StyleBuilder::new().size(side, side).build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_icon.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .a11y(ui_core::a11y::A11yOverride {
                    hidden: true,
                    ..ui_core::a11y::A11yOverride::default()
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    parent,
                )?;
            icon_slot_inner.set(Some(icon));
            let theme_text = theme.clone();
            let text = NodeBuilder::text(
                SmolStr::new_static(""),
                TextStyle::new(size.text_size())
                    .with_wrap(TextWrap::NoWrap)
                    .with_overflow(TextOverflow::Ellipsis, 1),
            )
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_text(move |cx| {
                let text = cx.read(&display).clone();
                if text.is_empty() {
                    placeholder.clone()
                } else {
                    text
                }
            })
            .bind_style(move |cx| {
                let foreground = if cx.read(&selected).is_none() {
                    theme_text.role(cx, Role::FgMuted)
                } else {
                    trigger_text_move.foreground_color(cx)
                };
                StylePatch {
                    foreground: Some(foreground),
                    ..StylePatch::default()
                }
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
            trigger_text.watch(text);
            let theme_caret = theme.clone();
            let caret = mount_dynamic_icon(
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
            trigger_caret.watch(caret);
            Ok(())
        })?;
        {
            let mut state = self.inner.state.borrow_mut();
            state.trigger = Some(node);
            state.trigger_icon = icon_slot.get();
            state.icon_shown = current_icon;
        }
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
    /// 同 [`Dropdown::build`] / [`Dropdown::attach`]。
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

    /// 打开面板（锚点 = 触发器；宽度 = 实测触发器宽度或显式宽度）。
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
            .set_content(app, self.panel_content())
            .map_err(OverlayError::from)?;
        let opened = self
            .inner
            .panel
            .open(app, window, Anchor::node(trigger), width)?;
        if opened {
            let row = app
                .read_with(&self.inner.selected, |selected| *selected)
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

    /// 帧同步：触发器状态 / 选中项图标 / 面板行 / 打开时宽度对齐触发器。
    ///
    /// # Errors
    /// 引擎列表同步错误。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.inner.trigger.sync(app)?;
        changed |= self.sync_trigger_a11y(app);
        changed |= self.sync_trigger_icon(app)?;
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

    /// 释放组件（触发器 / 面板的信号作用域与动作 handler）。
    pub fn release(&self, app: &mut App) -> usize {
        for action in [
            &self.inner.actions.trigger_click,
            &self.inner.actions.down,
            &self.inner.actions.up,
        ] {
            app.off_action(action);
        }
        self.inner.trigger.release(app) + self.inner.panel.release(app)
    }

    // ── 内部 ──

    /// 条目 → 面板行 / build / a11y 回调。
    fn panel_content(&self) -> PanelContent {
        let items = self.inner.items.borrow().clone();
        let rows: Vec<PanelRow> = items
            .iter()
            .map(|item| PanelRow {
                height: self.inner.options.size.h(),
                disabled: item.disabled,
            })
            .collect();
        let theme = self.inner.theme.clone();
        let size = self.inner.options.size;
        let labels: Vec<SmolStr> = items.iter().map(|item| item.label.clone()).collect();
        let icons = Rc::new(self.inner.icons.snapshot());
        let has_icon = items.iter().any(|item| item.icon.is_some());
        let build: Rc<dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>> = {
            let items = items.clone();
            Rc::new(move |tree, parent, index| {
                let Some(item) = items.get(index) else {
                    return Ok(());
                };
                build_icon_slot(tree, parent, &theme, &icons, item.icon, size, has_icon)?;
                let theme = theme.clone();
                NodeBuilder::text(
                    item.label.clone(),
                    TextStyle::new(size.text_size())
                        .with_wrap(TextWrap::NoWrap)
                        .with_overflow(TextOverflow::Ellipsis, 1),
                )
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme.role(cx, Role::FgDefault)),
                    ..StylePatch::default()
                })
                .build(tree, parent)
                .map(|_| ())
            })
        };
        let disabled: Vec<bool> = items.iter().map(|item| item.disabled).collect();
        let measure_labels = labels.clone();
        let a11y: Rc<dyn Fn(usize) -> A11yPatch> = Rc::new(move |index| match labels.get(index) {
            Some(label) => A11yPatch {
                role: Some(A11yRole::MenuItem),
                label: Some(label.clone()),
                disabled: disabled.get(index).copied(),
                ..A11yPatch::default()
            },
            None => A11yPatch {
                hidden: Some(true),
                ..A11yPatch::default()
            },
        });
        let measure = Some(PanelMeasure {
            labels: measure_labels,
            extra: if has_icon { size.icon_size() } else { 0.0 },
        });
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
            let row = app
                .read_with(&this.inner.selected, |selected| *selected)
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
        self.inner.panel.on_activate(move |app, row| {
            this.select_row(app, row);
        });
        let this = self.clone();
        self.inner.panel.on_close(move |app| {
            // 引擎侧关闭（点击外部 / Esc）后把焦点还给触发器。
            let (Some(window), Some(trigger)) = (
                this.inner.state.borrow().window,
                this.inner.state.borrow().trigger,
            ) else {
                return;
            };
            let _ = app.set_focus(window, Some(trigger));
        });
    }

    /// 选中一行（用户操作）。
    fn select_row(&self, app: &mut App, row: usize) {
        let item = self.inner.items.borrow().get(row).cloned();
        let Some(item) = item else {
            return;
        };
        if item.disabled {
            return;
        }
        self.apply_selected(app, Some(row));
        let _ = self.close(app);
        if let Some(callback) = self.inner.on_select.borrow().clone() {
            callback(app, item.id);
        }
    }

    /// 写选中 / 显示文本 / 触发器可访问名。
    fn apply_selected(&self, app: &mut App, index: Option<usize>) {
        let label = index
            .and_then(|index| {
                self.inner
                    .items
                    .borrow()
                    .get(index)
                    .map(|item| item.label.clone())
            })
            .unwrap_or_default();
        app.write(&self.inner.selected, index);
        app.write(&self.inner.display, label);
        self.sync_trigger_a11y(app);
        if let Some(index) = index {
            self.inner.panel.set_highlighted(app, Some(index));
        }
    }

    /// 触发器可访问名：显式 `label` 优先，否则 = 选中项文本 / 占位文本（随选中变化失效）。
    fn sync_trigger_a11y(&self, app: &mut App) -> bool {
        if self.inner.options.label.is_some() {
            return false;
        }
        let next = app
            .read_with(&self.inner.selected, |selected| *selected)
            .and_then(|index| {
                self.inner
                    .items
                    .borrow()
                    .get(index)
                    .map(|item| item.label.clone())
            })
            .unwrap_or_else(|| self.inner.options.placeholder.clone());
        app.write(&self.inner.a11y_label, next)
    }

    /// 当前选中项的图标。
    fn selected_icon(&self, app: &App) -> Option<IconKind> {
        let index = app.read_with(&self.inner.selected, |selected| *selected)?;
        self.inner
            .items
            .borrow()
            .get(index)
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

    /// 选中项变化 ⇒ 换触发器前导图标的资源与占位尺寸。
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

    /// 实测触发器宽度（打开期间对齐面板）。
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

impl fmt::Debug for Dropdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Dropdown")
            .field("key", &self.inner.key)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    //! 引擎树上的行为：触发器开合 / 键盘 / 选择 / 宽度对齐 / 长列表虚拟化 / 关闭与焦点 / a11y / I3。

    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::Duration,
    };

    use smol_str::SmolStr;
    use ui_core::{
        a11y::A11yRole,
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Point, Rect},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{Dropdown, DropdownItem, DropdownOptions};
    use crate::{
        kit::{
            icon::IconKind,
            layer::Layer,
            overlay::AnimationRouter,
            test_support::{a11y_snapshot_without_oracle, overlay_wrapper},
        },
        style_builder::StyleBuilder,
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        selected: Rc<RefCell<Vec<SmolStr>>>,
        background_clicks: Rc<Cell<usize>>,
    }

    impl Harness {
        fn new() -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let clicks = Rc::new(Cell::new(0));
            let sink = clicks.clone();
            NodeBuilder::boxed()
                .id("background")
                .layout_override(Rect::new(600.0, 500.0, 80.0, 40.0))
                .on_click(Action::Custom(SmolStr::new_static("background.click")))
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("背景节点");
            app.app_mut().on_action(
                Action::Custom(SmolStr::new_static("background.click")),
                move |_| sink.set(sink.get() + 1),
            );
            Self {
                app,
                window,
                theme,
                layer: Layer::new(),
                router: AnimationRouter::new(),
                selected: Rc::new(RefCell::new(Vec::new())),
                background_clicks: clicks,
            }
        }

        fn dropdown(&mut self, options: DropdownOptions) -> Dropdown {
            let selected = self.selected.clone();
            Dropdown::new(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                self.router.clone(),
                "dropdown",
                options,
            )
            .on_select(move |_, id| selected.borrow_mut().push(id))
        }

        fn mount(&mut self, dropdown: &Dropdown, items: impl IntoIterator<Item = DropdownItem>) {
            let root = self
                .app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .root();
            dropdown.set_items(self.app.app_mut(), items).expect("条目");
            dropdown
                .mount(self.app.app_mut(), self.window, root)
                .expect("挂载");
            self.settle(dropdown);
        }

        fn settle(&mut self, dropdown: &Dropdown) {
            for _ in 0..6 {
                self.app.advance(Duration::from_millis(40));
                let _ = self.app.drain().expect("帧");
                let _ = dropdown.sync(self.app.app_mut());
            }
            self.app.advance(Duration::from_millis(200));
            let _ = self.app.drain().expect("帧");
            let _ = dropdown.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
        }

        fn focus_trigger(&mut self, dropdown: &Dropdown) {
            let trigger = dropdown.trigger_node().expect("触发器");
            self.app.app_mut().set_focus(self.window, Some(trigger));
        }

        fn key(&mut self, dropdown: &Dropdown, code: KeyCode) {
            let _ = self
                .app
                .inject(InputEvent::key_down(code, Modifiers::empty()));
            let _ = self
                .app
                .inject(InputEvent::key_up(code, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
            let _ = dropdown.sync(self.app.app_mut());
        }

        fn click(&mut self, dropdown: &Dropdown, at: Point) {
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
            // 揭开面板要等两帧（`Overlay` 首次实测后才 `visible`，命中同样被挡）。
            self.settle(dropdown);
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
            self.find(&format!("dropdown:panel:list:row:{index}"))
                .unwrap_or_else(|| panic!("第 {index} 行未挂载"))
        }

        fn row_center(&self, index: usize) -> Point {
            let bounds = self.bounds(self.row(index));
            Point::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        /// 触发器里第一个文本叶子的当前文本（`bind_text` 的结果）。
        fn trigger_text(&self, dropdown: &Dropdown) -> SmolStr {
            let trigger = dropdown.trigger_node().expect("触发器");
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![trigger];
            while let Some(node) = stack.pop() {
                if let Ok(target) = tree.get(node) {
                    if let NodeKind::Text(content) = target.kind() {
                        return content.text.clone();
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            panic!("触发器里没有文本节点")
        }

        fn mounted_rows(&self) -> usize {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![tree.root()];
            let mut rows = 0;
            while let Some(node) = stack.pop() {
                if let Some(id) = tree.debug_id(node)
                    && id.starts_with("dropdown:panel:list:row:")
                    && !id.ends_with(":content")
                    && !id.contains(":hit:")
                {
                    rows += 1;
                }
                if let Ok(target) = tree.get(node) {
                    stack.extend(target.children().iter().copied());
                }
            }
            rows
        }

        fn idles(&mut self) -> bool {
            !self.app.drain().expect("帧").rendered
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
    }

    fn items() -> Vec<DropdownItem> {
        vec![
            DropdownItem::new("a", "Alpha"),
            DropdownItem::new("b", "Beta"),
            DropdownItem::new("c", "Gamma").disabled(true),
            DropdownItem::new("d", "Delta"),
        ]
    }

    #[test]
    fn pressing_the_trigger_opens_the_panel_and_pressing_it_again_closes_it() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        let center = harness.center(trigger);

        harness.click(&dropdown, center);
        assert!(dropdown.is_open(harness.app.app()), "点击触发器打开面板");
        assert!(dropdown.panel_node().is_some());

        harness.click(&dropdown, center);
        assert!(
            !dropdown.is_open(harness.app.app()),
            "再点触发器关闭面板（触发器在浮层外）"
        );
    }

    #[test]
    fn choosing_an_item_updates_the_selection_the_trigger_and_the_callback() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        let at = harness.row_center(1);
        harness.click(&dropdown, at);
        assert_eq!(
            harness.selected.borrow().as_slice(),
            &[SmolStr::new_static("b")],
            "选中项回调"
        );
        assert_eq!(
            dropdown.selected(harness.app.app()),
            Some(SmolStr::new_static("b"))
        );
        assert!(!dropdown.is_open(harness.app.app()), "选中即关面板");
        assert_eq!(harness.trigger_text(&dropdown), "Beta", "触发器显示选中项");
    }

    #[test]
    fn arrow_down_on_the_trigger_opens_with_the_selected_item_highlighted() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        dropdown.set_selected(harness.app.app_mut(), Some("d"));
        harness.settle(&dropdown);
        harness.focus_trigger(&dropdown);

        harness.key(&dropdown, KeyCode::ArrowDown);
        assert!(dropdown.is_open(harness.app.app()), "↓ 打开面板");
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(3),
            "高亮落选中项"
        );
    }

    #[test]
    fn end_moves_the_highlight_to_the_last_item_and_enter_chooses_it() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        harness.key(&dropdown, KeyCode::End);
        assert_eq!(dropdown.highlighted(harness.app.app()), Some(3));
        harness.key(&dropdown, KeyCode::Home);
        assert_eq!(dropdown.highlighted(harness.app.app()), Some(0));
        harness.key(&dropdown, KeyCode::End);
        harness.key(&dropdown, KeyCode::Enter);
        assert_eq!(
            harness.selected.borrow().as_slice(),
            &[SmolStr::new_static("d")]
        );
    }

    #[test]
    fn a_disabled_item_cannot_be_chosen_and_keeps_the_panel_open() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        let at = harness.row_center(2);
        harness.click(&dropdown, at);
        assert!(harness.selected.borrow().is_empty(), "禁用条目不可选");
        assert!(dropdown.is_open(harness.app.app()), "面板保持打开");
    }

    #[test]
    fn the_panel_is_as_wide_as_its_trigger() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        let trigger_width = harness.bounds(trigger).size.width;
        let panel = harness.bounds(dropdown.panel_node().expect("面板"));
        assert!(
            (panel.size.width - trigger_width).abs() < 0.5,
            "面板宽度对齐触发器：面板 {} / 触发器 {trigger_width}",
            panel.size.width
        );
    }

    #[test]
    fn a_long_item_list_is_virtualized_and_scrolls_to_the_end() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions {
            max_height: 200.0,
            ..DropdownOptions::default()
        });
        let many: Vec<DropdownItem> = (0..200)
            .map(|index| {
                let id = format!("item-{index}");
                DropdownItem::new(id.clone(), id)
            })
            .collect();
        harness.mount(&dropdown, many);
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        let panel = harness.bounds(dropdown.panel_node().expect("面板"));
        assert!(
            panel.size.height <= 200.0 + 0.5,
            "面板高度被 max_height 截住"
        );
        assert!(
            harness.mounted_rows() < 25,
            "长列表只挂可见窗口内的行（实际 {}）",
            harness.mounted_rows()
        );
        harness.key(&dropdown, KeyCode::End);
        assert_eq!(dropdown.highlighted(harness.app.app()), Some(199));
        assert!(harness.find("dropdown:panel:list:row:199").is_some());
    }

    #[test]
    fn a_press_outside_closes_the_panel_and_returns_focus_to_the_trigger() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()));

        harness.click(&dropdown, Point::new(640.0, 520.0));
        assert!(!dropdown.is_open(harness.app.app()), "点击外部关闭");
        assert_eq!(
            harness.background_clicks.get(),
            0,
            "外部按下被模态浮层消费（已接受偏差 F5：不穿透背景）"
        );
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger),
            "关闭后焦点回到触发器"
        );
    }

    #[test]
    fn escape_closes_the_panel() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        harness.key(&dropdown, KeyCode::Escape);
        assert!(!dropdown.is_open(harness.app.app()), "Esc 关闭面板");
    }

    /// 旧 `a_closed_dropdown_should_not_swallow_a_click_behind_it`：关闭态不遮背景。
    #[test]
    fn a_closed_dropdown_does_not_swallow_a_click_behind_it() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        harness.click(&dropdown, Point::new(640.0, 520.0));
        assert_eq!(harness.background_clicks.get(), 1, "关闭态不遮背景点击");
    }

    #[test]
    fn the_panel_declares_menu_and_menu_item_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let panel = dropdown.panel_node().expect("面板");
        // E12 之后角色落在引擎浮层包装节点上（面板本身是 GenericContainer）。
        let wrapper = overlay_wrapper(harness.app.app(), harness.window, panel);
        assert_eq!(
            snapshot.node_for(wrapper).expect("包装").role,
            A11yRole::Menu
        );
        assert_eq!(
            snapshot.node_for(panel).expect("面板").role,
            A11yRole::GenericContainer
        );
        let labels: Vec<String> = snapshot
            .nodes()
            .iter()
            .filter(|node| node.role == A11yRole::MenuItem)
            .filter_map(|node| node.label.as_ref().map(|label| label.text().to_owned()))
            .collect();
        assert!(
            labels.contains(&"Alpha".to_owned()),
            "行可访问名：{labels:?}"
        );
        assert!(labels.contains(&"Gamma".to_owned()), "禁用行也在 a11y 树里");
    }

    #[test]
    fn a_settled_dropdown_with_no_input_requests_no_frames() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        assert!(!dropdown.sync(harness.app.app_mut()).expect("同步"));
        assert!(harness.idles(), "稳态不出帧（I3）");
    }

    #[test]
    fn set_items_keeps_no_selection_and_shows_the_placeholder() {
        // 审查 F4：旧 dropdown 没有选中状态；首项回退只属于 select「打开时的高亮」。
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default().placeholder("请选择"));
        harness.mount(&dropdown, items());
        assert_eq!(
            dropdown.selected(harness.app.app()),
            None,
            "set_items 不自动选中首项"
        );
        assert_eq!(harness.trigger_text(&dropdown), "请选择", "占位文本可见");
    }

    #[test]
    fn a_long_item_grows_the_panel_past_its_trigger() {
        // 旧 `min_w(trigger)` 是下限：内容更宽时面板长到内容宽（不是精确等宽）。
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(
            &dropdown,
            vec![
                DropdownItem::new(
                    "long",
                    "A very long dropdown entry that cannot fit the trigger width at all",
                ),
                DropdownItem::new("short", "B"),
            ],
        );
        let trigger = dropdown.trigger_node().expect("触发器");
        let trigger_width = harness.bounds(trigger).size.width;
        harness.click(&dropdown, harness.center(trigger));
        let panel = harness.bounds(dropdown.panel_node().expect("面板"));
        assert!(
            panel.size.width > trigger_width + 100.0,
            "长条目把面板撑宽（旧 min_w 下限口径）：面板 {} / 触发器 {trigger_width}",
            panel.size.width
        );
    }

    #[test]
    fn item_icons_are_drawn_in_the_rows_and_on_the_trigger() {
        // 旧 dropdown 的条目是 `MenuItem`（可带 `IconAction`），条目图标列必须保留。
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(
            &dropdown,
            vec![
                DropdownItem::new("a", "Alpha").icon(IconKind::Circle),
                DropdownItem::new("b", "Beta"),
            ],
        );
        let trigger = dropdown.trigger_node().expect("触发器");
        assert_eq!(
            harness.visible_images(trigger),
            1,
            "没有选中项时触发器只有 caret"
        );

        harness.click(&dropdown, harness.center(trigger));
        let panel = dropdown.panel_node().expect("面板");
        assert_eq!(
            harness.visible_images(panel),
            1,
            "只有带图标的条目画图标（其余行留占位槽）"
        );

        harness.click(&dropdown, harness.row_center(0));
        harness.settle(&dropdown);
        assert_eq!(
            harness.visible_images(trigger),
            2,
            "选中带图标的条目后触发器画该图标 + caret"
        );
    }

    #[test]
    fn the_trigger_reports_the_selection_and_the_expanded_state() {
        // 审查 F8：触发器可访问名跟选中项走、展开态走 `aria-expanded`。
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dropdown = harness.dropdown(DropdownOptions::default().placeholder("请选择"));
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");

        harness.settle(&dropdown);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(record.label_text(), Some("请选择"), "占位文本是可访问名");
        assert_eq!(record.expanded, Some(false), "关闭态 aria-expanded=false");

        harness.click(&dropdown, harness.center(trigger));
        harness.settle(&dropdown);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.expanded, Some(true), "打开态 aria-expanded=true");

        harness.click(&dropdown, harness.row_center(1));
        harness.settle(&dropdown);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.label_text(), Some("Beta"), "可访问名跟随选中项");
        assert_eq!(record.expanded, Some(false), "关闭后 expanded 复原");
    }

    #[test]
    fn an_open_dropdown_consumes_background_scroll() {
        // 已接受的偏差（审查 F5）：菜单族是模态浮层，打开态外部滚轮被消费（背景不滚）。
        let mut harness = Harness::new();
        let root = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .root();
        let page = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("page")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(800.0)
                        .height(600.0)
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("页面")
        };
        let handle = harness
            .app
            .app_mut()
            .make_scroll(
                harness.window,
                page,
                ui_core::interaction::ScrollConfig::default(),
            )
            .expect("滚动容器");
        harness
            .app
            .app_mut()
            .set_scroll_content(&handle, ui_core::math::Size::new(800.0, 2400.0))
            .expect("内容");

        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()));

        let wheel = |harness: &mut Harness, dropdown: &Dropdown| {
            let _ = harness.app.inject(InputEvent::scroll_lines(
                Point::new(700.0, 500.0),
                Point::new(0.0, -2.0),
                Modifiers::empty(),
            ));
            harness.settle(dropdown);
            for _ in 0..8 {
                harness.app.advance(Duration::from_millis(40));
                let _ = harness.app.drain().expect("帧");
            }
        };
        wheel(&mut harness, &dropdown);
        assert_eq!(
            harness
                .app
                .app()
                .scroll_state(&handle)
                .expect("滚动状态")
                .offset
                .y,
            0.0,
            "面板打开时背景滚轮被消费"
        );

        // 对照：让触发器再点一次关掉面板后，同一位置的滚轮会滚背景。
        harness.click(&dropdown, harness.center(trigger));
        wheel(&mut harness, &dropdown);
        assert!(
            harness
                .app
                .app()
                .scroll_state(&handle)
                .expect("滚动状态")
                .offset
                .y
                > 50.0,
            "无浮层时背景可滚"
        );
    }
}

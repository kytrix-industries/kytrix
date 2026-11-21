//! 菜单族内核（P2-4c：旧 `crate::menu` 的引擎化重建 + dropdown / select / context_menu 的共享面板）。
//!
//! 旧实现由三块拼出来：`menu.rs`（无状态菜单列表，条目 = 幽灵按钮）、`overlay.rs`（定位 + 淡入淡出 +
//! 点击外部关闭）、`layer.rs`（优先级 / 单一 context menu）。旧菜单**没有**键盘导航、没有子菜单、
//! 没有选中态；本模块按 P2-4c 交付把它们补齐，并把「条目面板」抽成一份共享实现供
//! `dropdown` / `select` / `context_menu` 复用。
//!
//! # 结构
//!
//! ```text
//! Overlay（引擎浮层：placement / 点击外部关闭 / Esc → Action::Dismiss / 焦点陷阱 / 焦点恢复）
//! └─ panel（Overlay 面板：背景 / 圆角 / 内边距 / 阴影，a11y 角色 = Menu 或 List）
//!    └─ surface（本模块建的**键盘作用域**：tab_stop + 点击动作 + 固定宽高 + 裁剪）
//!       └─ List（`kit::list` 内核：虚拟化 + 变高实测 + 悬停/选中行样式 + 自绘滚动条）
//!          └─ row:content > hit（每行一个带点击动作的命中壳）> 应用内容（图标 / 文本 / 快捷键 / 勾）
//! ```
//!
//! [`ItemPanel`] 只认「行」这一件事：每行有高度、可禁用、一条 a11y 补丁，以及一个把内容建进
//! 命中壳的回调。菜单条目模型（[`MenuEntry`]）由 [`Menu`] 解释成行；下拉 / 选择的扁平选项由各自
//! 模块解释成行。行内核复用 [`crate::kit::list::List`]（E3 虚拟列表）：长菜单 / 长选项表只挂
//! 可见窗口内的行。
//!
//! # 键盘
//!
//! 作用域键（E8 `App::bind_key_scoped`）绑在**面板的 surface 节点**上，焦点在浮层内时生效：
//! `↑` / `↓` 移动高亮（跳过禁用 / 分组 / 分隔线行）、`Home` / `End` 到首 / 末可激活行、
//! `Enter` / `Space` 激活高亮行（surface 的点击动作；指针点击走行自己的点击动作）、
//! 菜单额外有 `→` 打开子菜单 / `←` 关闭本层。`Esc` 由引擎默认绑定 → `Action::Dismiss` →
//! 卸载**最上层**浮层（子菜单先关），本模块的 `sync` 探测到引擎卸载后收尾。
//!
//! 指针点击行时引擎按「最近祖先的动作」派发到行命中壳的动作（`ActionSource::Click`），行号由
//! 命中节点向上反查 [`crate::kit::list::List::mounted_rows`]。行命中壳的点击在引擎侧就完成，
//! 不依赖外壳转发事件；[`ItemPanel::dispatch`] 只是点击落在行内非动作节点时的兜底（与
//! `interactive` 的按下态同一约定：外壳在 `App::drain_events` 之后转发）。
//!
//! # a11y（E9 集合语义）
//!
//! 面板声明 [`MenuMode::Menu`]（`Menu` + `MenuItem`）或 [`MenuMode::ListBox`]（`List` +
//! `ListBoxOption`）：角色与标签打在**浮层面板 / 列表行**上（行基线由 `List::set_row_a11y`
//! 叠加），行内的命中壳与绘制节点声明 `hidden`，避免 AT 把同一个标签读两遍。（E9 的虚拟列表
//! 容器恒为 `List`，因此菜单的 a11y 嵌套是 `Menu > List > MenuItem`；登记见 spec-delta。）
//!
//! # 与引擎语义的差异（登记见 `.docs/gui-migration/spec-deltas/kit-menu.md`）
//!
//! - 引擎没有 `ComboBox` / `ListBox` 角色：选择器用 `Button`（触发器）+ `List` / `ListBoxOption`
//!   表达，`aria-expanded` 由触发器的 `A11yPatch` 承担；
//! - 引擎没有组不透明度（E13）：淡入淡出作用在浮层壳上，行内容不跟随（旧实现同样是整层消隐）；
//! - 滚动条是 `kit::list` 内核的自绘纵向条（旧菜单 `overflow_y_scroll` + 自绘条的同义）；
//! - 菜单条目**没有**任意自定义元素通道（旧 `MenuItem::Custom`）：引擎的应用层组件是挂载期
//!   建树，行内容必须可由回调重建，`AnyElement` 这种一次性元素没有落点。

use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::{Rc, Weak},
    time::Duration,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::{App, TimerId},
    error::EngineError,
    event::{
        Action, ActionSource, Event, InputEvent, KeyBinding, KeyCode, Modifiers, PointerButton,
    },
    interaction::{ActionCtx, ItemAlign},
    math::{Point, Size},
    node::{
        builder::NodeBuilder, dependency::ReadCtx, id::NodeId, image::ImageFit, signal::Signal,
        tree::NodeTree,
    },
    paint::style::StylePatch,
    resources::ResourceId,
    text::{TextStyle, TextWrap},
    window::WindowId,
};

use crate::{
    kit::{
        icon::{IconKind, RasterTier, raster},
        layer::{Band, Layer},
        list::{List, ListOptions, RowHeight, Selection, SelectionMode},
        overlay::{
            Anchor, DismissPolicy, Overlay, OverlayA11y, OverlayError, OverlayOptions,
            OverlaySurface, Placement,
        },
        size::UnifiedSize,
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};

/// 面板内边距（逻辑像素；旧 `Menu` 的 `.p_1()`，也是行内容的左右留白）。
const MENU_PADDING: f32 = 4.0;
/// 分组标题行高比例（相对尺寸档高度）。
const GROUP_HEIGHT_RATIO: f32 = 0.75;
/// 分隔线行高（逻辑像素）。
const SEPARATOR_HEIGHT: f32 = 9.0;
/// 子菜单与父菜单行的间隙（贴边 0 更像桌面菜单）。
const SUBMENU_GAP: f32 = 0.0;
/// 悬停打开子菜单的延迟（E10 一次性定时器）。
const SUBMENU_DELAY: Duration = Duration::from_millis(150);

// ── 图标资源 ──────────────────────────────────────────────────────────────

/// 图标资源缓存：行内容建树时拿不到 `App`（`List` 的行工厂只有 `NodeTree`），所以图标资源
/// 必须在打开面板前用 `App` 解析成 [`ResourceId`]，行内只引用 id。
///
/// 键是 `(图标, 逻辑尺寸, 光栅档)`：同一图标在 1× / 2× 下各存一份（DPI 换档后重新解析）。
#[derive(Default)]
struct IconCache {
    entries: RefCell<Vec<CachedIcon>>,
}

struct CachedIcon {
    kind: IconKind,
    size: f32,
    tier: RasterTier,
    resource: ResourceId,
}

impl IconCache {
    /// 解析（必要时上传）一个图标的资源 id。
    ///
    /// # Errors
    /// `UnknownWindow`（窗口已关闭）/ 图片解码错误。
    fn resolve(
        &self,
        app: &mut App,
        window: WindowId,
        kind: IconKind,
        size: f32,
    ) -> Result<(), EngineError> {
        let scale = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .scale()
            .factor();
        let tier = RasterTier::for_scale(scale);
        if self
            .entries
            .borrow()
            .iter()
            .any(|entry| entry.kind == kind && entry.size == size && entry.tier == tier)
        {
            return Ok(());
        }
        let bytes = raster(kind, tier);
        let target = Size::new(size * scale, size * scale);
        let image = app
            .resources_mut()
            .insert_owned_at_size(window, bytes, target)?;
        self.entries.borrow_mut().push(CachedIcon {
            kind,
            size,
            tier,
            resource: image.id,
        });
        Ok(())
    }
}

/// 行内的图标节点（行内容建树期可用：资源已解析成 id；`hidden` = 装饰）。
fn build_icon(
    tree: &mut NodeTree,
    parent: NodeId,
    resource: ResourceId,
    size: f32,
    bind_style: impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static,
) -> Result<(), EngineError> {
    NodeBuilder::image(resource, ImageFit::Fill)
        .style(StyleBuilder::new().size(size, size).build())
        .bind_style(bind_style)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)
        .map(|_| ())
}

// ── 共享面板 ──────────────────────────────────────────────────────────────

/// 面板的一行（内核只需要高度 / 可禁用；内容由应用回调建）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanelRow {
    /// 行高（逻辑像素）。
    pub height: f32,
    /// 不可交互（分组标题 / 分隔线 / 禁用条目）：键盘导航跳过、点击不激活。
    pub disabled: bool,
}

/// 面板内容：行规格 + 建行内容 + 行 a11y 补丁。
pub(crate) struct PanelContent {
    /// 行规格（顺序 = 行号）。
    pub rows: Vec<PanelRow>,
    /// 把第 `index` 行的内容建进 `parent`（命中壳）。
    pub build: Rc<dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>>,
    /// 行 a11y 补丁（角色 / 标签 / 选中 / 禁用 / 勾选）。
    pub a11y: Rc<dyn Fn(usize) -> A11yPatch>,
}

impl Clone for PanelContent {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            build: self.build.clone(),
            a11y: self.a11y.clone(),
        }
    }
}

/// 面板的角色对（菜单 / 列表盒两套语义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PanelRoles {
    /// 浮层面板角色（`Menu` / `List`）。
    pub panel: A11yRole,
    /// 行角色（`MenuItem` / `ListBoxOption`）。
    pub row: A11yRole,
}

/// 共享面板参数。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ItemPanelOptions {
    /// 尺寸档（行高 / 内边距 / 字号）。
    pub size: UnifiedSize,
    /// 列表视口最大高度（逻辑像素；超出滚动）。
    pub max_height: f32,
    /// 行高估计（变高列表的首帧值，实际值由实测回写）。
    pub row_height: f32,
    /// 层带。
    pub band: Band,
    /// 是否模态（焦点陷阱 + 浮层外按下被消费）。根菜单 / 下拉 / 选择恒 `true`。
    pub modal: bool,
    /// 定位。
    pub placement: Placement,
    /// 与锚点的间隙。
    pub gap: f32,
    /// a11y 角色对。
    pub roles: PanelRoles,
    /// 面板可访问名。
    pub label: Option<SmolStr>,
}

/// 面板错误（浮层参数 / 引擎建树两条来源，都是一个 `OverlayError`）。
type PanelResult<T> = Result<T, OverlayError>;

/// 键盘 / 点击动作身份。
struct PanelActions {
    up: Action,
    down: Action,
    home: Action,
    end: Action,
    /// surface 的点击动作（键盘 `Enter` / `Space`：引擎按 Click 派发到焦点节点的动作）。
    activate: Action,
    /// 行命中壳的点击动作（指针点击；行号由命中节点反查）。
    row: Action,
}

struct PanelState {
    window: Option<WindowId>,
    surface: Option<NodeId>,
    width: f32,
    height: f32,
    opened: bool,
}

struct PanelInner {
    key: SmolStr,
    options: ItemPanelOptions,
    overlay: Overlay,
    list: List,
    /// `List` 的行工厂拿不到 `App`：内容经这个共享槽位进内核。
    content: Rc<RefCell<Option<PanelContent>>>,
    actions: PanelActions,
    on_activate: RefCell<Option<Rc<dyn Fn(&mut App, usize)>>>,
    on_close: RefCell<Option<Rc<dyn Fn(&mut App)>>>,
    state: RefCell<PanelState>,
    /// 是否打开（触发器画按下态 / 组合控件读）。
    open: Signal<bool>,
}

/// 共享条目面板（`Rc` 浅拷贝）。
#[derive(Clone)]
pub(crate) struct ItemPanel {
    inner: Rc<PanelInner>,
}

impl ItemPanel {
    /// 建面板（关闭态）：浮层 + 列表内核 + 动作身份。
    pub(crate) fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        key: impl Into<SmolStr>,
        options: ItemPanelOptions,
    ) -> Self {
        let key = key.into();
        let content: Rc<RefCell<Option<PanelContent>>> = Rc::new(RefCell::new(None));
        let list_options = ListOptions {
            row_height: RowHeight::Variable {
                estimate: options.row_height,
            },
            overscan: 4,
            selection: SelectionMode::Single,
            capacity: None,
            reverse: false,
            keep_on_newest: false,
            padding_x: MENU_PADDING,
            padding_y: 0.0,
            row_stretch: true,
            scrollbar: true,
        };
        let row_action = Action::Custom(format_smolstr!("{key}.row"));
        let list = {
            let shared = content.clone();
            let list_key = format_smolstr!("{key}:list");
            let row_action = row_action.clone();
            List::new(
                app,
                theme.clone(),
                list_key.clone(),
                list_options,
                move |tree, parent, index| {
                    let built = shared.borrow().as_ref().and_then(|content| {
                        content
                            .rows
                            .get(index)
                            .map(|row| (content.build.clone(), row.height))
                    });
                    let Some((build, height)) = built else {
                        return Ok(());
                    };
                    // 命中壳：整行可点、满宽；a11y `hidden`（行语义在列表行基线上）。
                    let hit = NodeBuilder::boxed()
                        .id(format_smolstr!("{list_key}:hit:{index}"))
                        .style(
                            StyleBuilder::new()
                                .flex_row()
                                .items_center()
                                .width_full()
                                .height(height)
                                .min_width_0()
                                .build(),
                        )
                        .on_click(row_action.clone())
                        .a11y(A11yOverride {
                            hidden: true,
                            ..A11yOverride::default()
                        })
                        .build(tree, parent)?;
                    build(tree, hit, index)
                },
            )
        };
        let overlay = Overlay::new(
            app,
            theme,
            layer,
            key.clone(),
            OverlayOptions {
                placement: options.placement,
                gap: options.gap,
                margin: 4.0,
                flip: true,
                dismiss: DismissPolicy::OutsideClick,
                modal: options.modal,
                restore_focus: true,
                band: options.band,
                surface: OverlaySurface::menu(),
                scrim: None,
                a11y: OverlayA11y {
                    role: Some(options.roles.panel),
                    label: options.label.clone(),
                },
                fade: true,
            },
        );
        let open = overlay.open_state();
        let inner = Rc::new(PanelInner {
            actions: PanelActions {
                up: Action::Custom(format_smolstr!("{key}.up")),
                down: Action::Custom(format_smolstr!("{key}.down")),
                home: Action::Custom(format_smolstr!("{key}.home")),
                end: Action::Custom(format_smolstr!("{key}.end")),
                activate: Action::Custom(format_smolstr!("{key}.activate")),
                row: row_action,
            },
            key,
            options,
            overlay,
            list,
            content,
            on_activate: RefCell::new(None),
            on_close: RefCell::new(None),
            state: RefCell::new(PanelState {
                window: None,
                surface: None,
                width: 0.0,
                height: 0.0,
                opened: false,
            }),
            open,
        });
        let panel = Self { inner };
        panel.register_actions(app);
        panel
    }

    /// 浮层句柄。
    pub(crate) fn overlay(&self) -> &Overlay {
        &self.inner.overlay
    }

    /// 列表内核（组合控件读挂载行 / 悬停）。
    pub(crate) fn list(&self) -> &List {
        &self.inner.list
    }

    /// 打开状态信号。
    pub(crate) fn open_state(&self) -> Signal<bool> {
        self.inner.open
    }

    /// 是否打开（应用语义；引擎侧关闭后要等一次 `sync` 才为 `false`）。
    pub(crate) fn is_open(&self, app: &App) -> bool {
        self.inner.overlay.is_open(app)
    }

    /// surface（键盘作用域）节点。
    pub(crate) fn surface_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().surface
    }

    /// 行数。
    pub(crate) fn row_count(&self) -> usize {
        self.inner
            .content
            .borrow()
            .as_ref()
            .map_or(0, |content| content.rows.len())
    }

    /// 行是否禁用 / 越界。
    pub(crate) fn row_disabled(&self, index: usize) -> bool {
        self.inner
            .content
            .borrow()
            .as_ref()
            .and_then(|content| content.rows.get(index))
            .is_none_or(|row| row.disabled)
    }

    /// 当前高亮行（引擎列表选中集的首项）。
    pub(crate) fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.list.selection(app).first()
    }

    /// 悬停行（`List` 的悬停镜像；未打开时为 `None`）。
    pub(crate) fn hovered(&self, app: &App) -> Option<usize> {
        app.read_with(&self.inner.list.hovered_signal(), |hovered| *hovered)
    }

    /// 设置内容（行数 / 行内容 / 行 a11y）。已打开时重建行。
    ///
    /// # Errors
    /// 引擎虚拟列表失效。
    pub(crate) fn set_content(
        &self,
        app: &mut App,
        content: PanelContent,
    ) -> Result<(), EngineError> {
        let count = content.rows.len();
        let disabled: Vec<usize> = content
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.disabled.then_some(index))
            .collect();
        let a11y = content.a11y.clone();
        {
            let mut slot = self.inner.content.borrow_mut();
            *slot = Some(content);
        }
        self.inner.list.set_disabled_rows(disabled);
        self.inner.list.set_row_a11y(move |index| a11y(index));
        if self.inner.overlay.is_open(app) {
            self.inner.list.set_items(app, count)?;
            self.apply_geometry(app)?;
            self.select_first(app);
        }
        Ok(())
    }

    /// 打开面板并把内容接进引擎。
    ///
    /// 返回 `true` = 本次真的打开了；`false` = 本来就开着（只更新锚点）。
    ///
    /// # Errors
    /// 浮层参数非法 / 引擎建树 / 虚拟列表接线失败。
    pub(crate) fn open(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        width: f32,
    ) -> PanelResult<bool> {
        let content = self.inner.content.borrow().clone();
        let Some(content) = content else {
            return Ok(false);
        };
        // `width` / `max_height` 是**面板**（含内边距）的口径：surface 扣掉两侧内边距。
        let width = (width.max(1.0) - MENU_PADDING * 2.0).max(1.0);
        let total: f32 = content.rows.iter().map(|row| row.height).sum();
        let height = total
            .min((self.inner.options.max_height - MENU_PADDING * 2.0).max(1.0))
            .max(0.0);
        let key = self.inner.key.clone();
        let list = self.inner.list.clone();
        let activate = self.inner.actions.activate.clone();
        let surface_slot = Rc::new(Cell::new(None));
        let slot = surface_slot.clone();
        let opened = self
            .inner
            .overlay
            .open(app, window, anchor, move |tree, panel| {
                let surface = NodeBuilder::boxed()
                    .id(format_smolstr!("{key}:surface"))
                    .style(
                        StyleBuilder::new()
                            .flex_col()
                            .width(width)
                            .height(height)
                            .min_width_0()
                            .min_height_0()
                            .overflow_hidden()
                            .build(),
                    )
                    .tab_stop(true)
                    .on_click(activate)
                    .a11y(A11yOverride {
                        role: Some(A11yRole::GenericContainer),
                        ..A11yOverride::default()
                    })
                    .build(tree, panel)?;
                slot.set(Some(surface));
                list.build(tree, surface)?;
                Ok(())
            })?;
        if !opened {
            self.resize(app, width)?;
            return Ok(false);
        }
        self.inner.list.attach(app, window)?;
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = Some(window);
            state.surface = surface_slot.get();
            state.width = width;
            state.height = height;
            state.opened = true;
        }
        self.inner.list.set_items(app, content.rows.len())?;
        self.bind_keys(app, window)?;
        self.select_first(app);
        Ok(true)
    }

    /// 只改宽度（宽度对齐触发器的路径；高度由行高之和决定）。
    ///
    /// # Errors
    /// 引擎建树错误。
    pub(crate) fn resize(&self, app: &mut App, width: f32) -> Result<(), EngineError> {
        // `width` 是面板口径（含内边距），surface 扣掉两侧内边距（与 `open` 一致）。
        let width = (width.max(1.0) - MENU_PADDING * 2.0).max(1.0);
        let (window, surface, previous) = {
            let state = self.inner.state.borrow();
            (state.window, state.surface, state.width)
        };
        let (Some(window), Some(surface)) = (window, surface) else {
            return Ok(());
        };
        if previous > 0.0 && (previous - width).abs() < 0.5 {
            return Ok(());
        }
        if let Some(tree) = app.window_mut(window).map(|target| target.tree_mut())
            && let Ok(node) = tree.get(surface)
        {
            let mut style = node.style().clone();
            style.inner.size.width = taffy::style::Dimension::length(width);
            tree.set_style(surface, style)?;
        }
        self.inner.state.borrow_mut().width = width;
        Ok(())
    }

    /// 关闭面板（有淡出时先淡出，由 `sync` 收尾）。
    pub(crate) fn close(&self, app: &mut App) -> bool {
        self.inner.overlay.close(app)
    }

    /// 立即卸载面板。
    pub(crate) fn close_now(&self, app: &mut App) -> bool {
        let closed = self.inner.overlay.close_now(app);
        self.finish_close(app);
        closed
    }

    /// 激活回调（点击行 / 回车激活高亮行；参数 = 行号）。
    pub(crate) fn on_activate(&self, callback: impl Fn(&mut App, usize) + 'static) {
        *self.inner.on_activate.borrow_mut() = Some(Rc::new(callback));
    }

    /// 关闭回调（引擎侧被点击外部 / Esc 关闭也调用；每次打开最多一次）。
    pub(crate) fn on_close(&self, callback: impl Fn(&mut App) + 'static) {
        *self.inner.on_close.borrow_mut() = Some(Rc::new(callback));
    }

    /// 设置高亮行（写列表选中集 + 保持可见）。
    pub(crate) fn set_highlighted(&self, app: &mut App, index: Option<usize>) {
        let index = index.filter(|index| !self.row_disabled(*index));
        let mut selection = Selection::new();
        if let Some(index) = index {
            selection.select(index);
        }
        if app.write(&self.inner.list.selection_signal(), selection)
            && let Some(index) = index
        {
            let _ = self
                .inner
                .list
                .scroll_to_item(app, index, ItemAlign::Nearest);
        }
    }

    /// 高亮首个可激活行（打开时 / 内容重建后）。
    pub(crate) fn select_first(&self, app: &mut App) {
        let next = (0..self.row_count()).find(|index| !self.row_disabled(*index));
        self.set_highlighted(app, next);
    }

    /// 激活一行：跳过禁用行，写高亮，再回调。
    pub(crate) fn activate(&self, app: &mut App, index: usize) -> bool {
        if self.row_disabled(index) || index >= self.row_count() {
            return false;
        }
        self.set_highlighted(app, Some(index));
        let callback = self.inner.on_activate.borrow().clone();
        match callback {
            Some(callback) => {
                callback(app, index);
                true
            }
            None => false,
        }
    }

    /// 焦点给 surface（打开后调用；浮层首帧还不可见时由 `sync` 重试）。
    pub(crate) fn focus(&self, app: &mut App) -> bool {
        let (Some(window), Some(surface)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().surface,
        ) else {
            return false;
        };
        app.set_focus(window, Some(surface))
    }

    /// 帧同步：浮层跟随锚点 / 外部关闭探测 / 列表行内容与行高 / 焦点声明。
    ///
    /// `claim_focus`：打开的子菜单存在时父面板不得把焦点抢回来。
    ///
    /// # Errors
    /// 引擎列表同步错误。
    pub(crate) fn sync(&self, app: &mut App, claim_focus: bool) -> Result<bool, EngineError> {
        let mut changed = self.inner.overlay.sync(app);
        if self.inner.state.borrow().opened && !self.inner.overlay.is_open(app) {
            // 引擎侧卸载（点击外部 / Esc / `Action::Dismiss`）：收尾一次。
            self.finish_close(app);
            return Ok(true);
        }
        if !self.inner.overlay.is_open(app) {
            return Ok(changed);
        }
        changed |= self.inner.list.sync(app)?;
        if claim_focus {
            changed |= self.claim_focus(app);
        }
        Ok(changed)
    }

    /// 焦点不在浮层内 ⇒ 给 surface（浮层揭晓前 `set_focus` 会失败，下一帧重试）。
    fn claim_focus(&self, app: &mut App) -> bool {
        let (Some(window), Some(surface)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().surface,
        ) else {
            return false;
        };
        let Some(root) = self.inner.overlay.overlay_node() else {
            return false;
        };
        if let Some(focused) = app.focused(window)
            && let Some(tree) = app.window(window).map(|target| target.tree())
        {
            let mut cursor = Some(focused);
            while let Some(node) = cursor {
                if node == root {
                    return false;
                }
                cursor = tree.get(node).ok().and_then(|target| target.parent());
            }
        }
        app.set_focus(window, Some(surface))
    }

    /// 转发平台事件：主键抬起落在某一行上 ⇒ 激活该行（兜底路径，见模块文档）。
    pub(crate) fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let Event::Input(InputEvent::PointerUp {
            position,
            button: PointerButton::Primary,
            ..
        }) = event
        else {
            return false;
        };
        if !self.inner.overlay.is_open(app) {
            return false;
        }
        let Some(window) = self.inner.state.borrow().window else {
            return false;
        };
        let Some(index) = self.row_at_point(app, window, *position) else {
            return false;
        };
        self.activate(app, index)
    }

    /// 释放面板（先卸载浮层节点，再释放信号作用域与动作 handler）。
    pub(crate) fn release(&self, app: &mut App) -> usize {
        self.close_now(app);
        for action in [
            &self.inner.actions.up,
            &self.inner.actions.down,
            &self.inner.actions.home,
            &self.inner.actions.end,
            &self.inner.actions.activate,
            &self.inner.actions.row,
        ] {
            app.off_action(action);
        }
        self.inner.overlay.release(app);
        self.inner.list.release(app)
    }

    /// 命中点所在的行号。
    fn row_at_point(&self, app: &App, window: WindowId, point: Point) -> Option<usize> {
        let node = app.window(window)?.tree().hit_test(point)?.node;
        self.row_at_node(app, window, node)
    }

    /// 从命中节点向上找行容器，返回行号。
    fn row_at_node(&self, app: &App, window: WindowId, node: NodeId) -> Option<usize> {
        let rows = self.inner.list.mounted_rows(app);
        let tree = app.window(window)?.tree();
        let mut cursor = Some(node);
        while let Some(current) = cursor {
            if let Some((index, _)) = rows.iter().find(|(_, row)| *row == current) {
                return Some(*index);
            }
            cursor = tree.get(current).ok().and_then(|target| target.parent());
        }
        None
    }

    /// 注册 6 个动作的 handler（面板自有的动作身份）。
    fn register_actions(&self, app: &mut App) {
        let this = self.clone();
        app.on_action(self.inner.actions.up.clone(), move |ctx| {
            this.move_highlight(ctx.app_mut(), -1);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.down.clone(), move |ctx| {
            this.move_highlight(ctx.app_mut(), 1);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.home.clone(), move |ctx| {
            this.jump_highlight(ctx.app_mut(), false);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.end.clone(), move |ctx| {
            this.jump_highlight(ctx.app_mut(), true);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.activate.clone(), move |ctx| {
            // 指针点击落不到 surface（行有更近的动作）：这里只处理键盘激活。
            if matches!(ctx.source(), ActionSource::Click { .. }) && ctx.hit().is_some() {
                return;
            }
            if let Some(index) = this.highlighted(ctx.app_mut()) {
                this.activate(ctx.app_mut(), index);
            }
        });
        let this = self.clone();
        app.on_action(self.inner.actions.row.clone(), move |ctx| {
            let Some(hit) = ctx.hit().map(|hit| hit.node) else {
                return;
            };
            let window = ctx.window();
            let app = ctx.app_mut();
            let Some(index) = this.row_at_node(app, window, hit) else {
                return;
            };
            this.activate(app, index);
        });
    }

    /// 绑定作用域键（每次打开后绑到新的 surface 节点；节点卸载时引擎自动清除）。
    fn bind_keys(&self, app: &mut App, window: WindowId) -> PanelResult<()> {
        let Some(surface) = self.inner.state.borrow().surface else {
            return Ok(());
        };
        let bindings = [
            (KeyCode::ArrowUp, self.inner.actions.up.clone()),
            (KeyCode::ArrowDown, self.inner.actions.down.clone()),
            (KeyCode::Home, self.inner.actions.home.clone()),
            (KeyCode::End, self.inner.actions.end.clone()),
        ];
        for (key, action) in bindings {
            app.bind_key_scoped(
                window,
                surface,
                KeyBinding {
                    key,
                    modifiers: Modifiers::empty(),
                    action,
                },
            )?;
        }
        Ok(())
    }

    /// 键盘移动高亮（`step` = ±1）；不环绕。
    fn move_highlight(&self, app: &mut App, step: isize) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let current = self.highlighted(app);
        let next = match (current, step.is_positive()) {
            (Some(current), true) => (current + 1..count).find(|index| !self.row_disabled(*index)),
            (Some(current), false) => (0..current).rev().find(|index| !self.row_disabled(*index)),
            (None, true) => (0..count).find(|index| !self.row_disabled(*index)),
            (None, false) => (0..count).rev().find(|index| !self.row_disabled(*index)),
        };
        if next.is_some() {
            self.set_highlighted(app, next);
        }
    }

    /// `Home` / `End`：首个 / 末个可激活行。
    fn jump_highlight(&self, app: &mut App, last: bool) {
        let count = self.row_count();
        let next = if last {
            (0..count).rev().find(|index| !self.row_disabled(*index))
        } else {
            (0..count).find(|index| !self.row_disabled(*index))
        };
        if next.is_some() {
            self.set_highlighted(app, next);
        }
    }

    /// 几何回写（内容变化 / 宽度变化后）。
    fn apply_geometry(&self, app: &mut App) -> Result<(), EngineError> {
        let (Some(window), Some(surface)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().surface,
        ) else {
            return Ok(());
        };
        let content = self.inner.content.borrow().clone();
        let Some(content) = content else {
            return Ok(());
        };
        let total: f32 = content.rows.iter().map(|row| row.height).sum();
        let height = total
            .min((self.inner.options.max_height - MENU_PADDING * 2.0).max(1.0))
            .max(0.0);
        let width = self.inner.state.borrow().width;
        self.inner.state.borrow_mut().height = height;
        if let Some(tree) = app.window_mut(window).map(|target| target.tree_mut())
            && let Ok(node) = tree.get(surface)
        {
            let mut style = node.style().clone();
            style.inner.size.width = taffy::style::Dimension::length(width);
            style.inner.size.height = taffy::style::Dimension::length(height);
            tree.set_style(surface, style)?;
        }
        Ok(())
    }

    /// 关闭收尾（引擎卸载后的状态 / 列表选中集 / 关闭回调）。
    fn finish_close(&self, app: &mut App) {
        let was_opened = {
            let mut state = self.inner.state.borrow_mut();
            let opened = state.opened;
            state.opened = false;
            state.surface = None;
            state.window = None;
            opened
        };
        let _ = app.write(&self.inner.open, false);
        let _ = self.inner.list.clear_selection(app);
        if was_opened && let Some(callback) = self.inner.on_close.borrow().clone() {
            callback(app);
        }
    }
}

impl fmt::Debug for ItemPanel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ItemPanel")
            .field("key", &self.inner.key)
            .field("rows", &self.row_count())
            .finish()
    }
}

// ── 菜单条目模型 ──────────────────────────────────────────────────────────

/// 一个可激活的菜单条目（旧 `MenuItemAction` / `MenuItemIconAction` 的并集）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuAction {
    /// 条目 id（回调里报告给调用方）。
    pub id: SmolStr,
    /// 显示文本（同时是可访问名）。
    pub label: SmolStr,
    /// 图标（画在文本左侧）。
    pub icon: Option<IconKind>,
    /// 快捷键提示（右对齐的弱化文本；旧实现没有这个字段）。
    pub shortcut: Option<SmolStr>,
    /// 禁用。
    pub disabled: bool,
    /// 危险条目（前景走 `StatusDangerBase`）。
    pub danger: bool,
    /// 勾选态（`None` = 非勾选型条目；选择器的多选条目用它）。
    pub checked: Option<bool>,
}

impl MenuAction {
    /// 文本条目。
    #[must_use]
    pub fn new(id: impl Into<SmolStr>, label: impl Into<SmolStr>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            shortcut: None,
            disabled: false,
            danger: false,
            checked: None,
        }
    }

    /// 图标。
    #[must_use]
    pub fn icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    /// 快捷键提示。
    #[must_use]
    pub fn shortcut(mut self, shortcut: impl Into<SmolStr>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// 禁用。
    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// 危险条目。
    #[must_use]
    pub fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    /// 勾选态。
    #[must_use]
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }
}

/// 子菜单（旧实现没有；挂一层嵌套浮层）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuSubmenu {
    /// 显示文本。
    pub label: SmolStr,
    /// 图标。
    pub icon: Option<IconKind>,
    /// 禁用（不打开）。
    pub disabled: bool,
    /// 子条目。
    pub entries: Vec<MenuEntry>,
}

impl MenuSubmenu {
    /// 子菜单。
    #[must_use]
    pub fn new(label: impl Into<SmolStr>, entries: impl IntoIterator<Item = MenuEntry>) -> Self {
        Self {
            label: label.into(),
            icon: None,
            disabled: false,
            entries: entries.into_iter().collect(),
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

/// 菜单条目（旧 `MenuItem` 5 个变体的引擎化版本）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    /// 可激活条目。
    Action(MenuAction),
    /// 分组标题（不可交互）。
    Group(SmolStr),
    /// 分隔线。
    Separator,
    /// 子菜单。
    Submenu(MenuSubmenu),
}

impl MenuEntry {
    /// 这个条目是否可激活（键盘导航 / 点击目标）。
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        match self {
            Self::Action(action) => !action.disabled,
            Self::Submenu(submenu) => !submenu.disabled,
            Self::Group(_) | Self::Separator => false,
        }
    }
}

/// 面板语义（a11y 角色对）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuMode {
    /// 菜单：`Menu` + `MenuItem`（默认）。
    #[default]
    Menu,
    /// 列表盒：`List` + `ListBoxOption`（选择器的面板）。
    ListBox,
}

impl MenuMode {
    fn roles(self) -> PanelRoles {
        match self {
            Self::Menu => PanelRoles {
                panel: A11yRole::Menu,
                row: A11yRole::MenuItem,
            },
            Self::ListBox => PanelRoles {
                panel: A11yRole::List,
                row: A11yRole::ListBoxOption,
            },
        }
    }
}

/// 菜单参数。
#[derive(Clone, Debug, PartialEq)]
pub struct MenuOptions {
    /// 尺寸档。
    pub size: UnifiedSize,
    /// 面板宽度（逻辑像素；旧菜单宽度来自外层 dropdown / context menu）。
    pub width: f32,
    /// 列表视口最大高度（超出滚动）。
    pub max_height: f32,
    /// 面板语义。
    pub mode: MenuMode,
    /// 根面板相对锚点的定位。
    pub placement: Placement,
    /// 与锚点的间隙。
    pub gap: f32,
    /// 层带。
    pub band: Band,
    /// 面板可访问名。
    pub label: Option<SmolStr>,
    /// 悬停打开子菜单的延迟（`ZERO` = 立即）。
    pub submenu_delay: Duration,
}

impl Default for MenuOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            width: 200.0,
            max_height: 320.0,
            mode: MenuMode::Menu,
            placement: Placement::BOTTOM_START,
            gap: 4.0,
            band: Band::Popup,
            label: None,
            submenu_delay: SUBMENU_DELAY,
        }
    }
}

impl MenuOptions {
    /// 面板宽度。
    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// 视口最大高度。
    #[must_use]
    pub fn max_height(mut self, max_height: f32) -> Self {
        self.max_height = max_height;
        self
    }

    /// 面板语义。
    #[must_use]
    pub fn mode(mut self, mode: MenuMode) -> Self {
        self.mode = mode;
        self
    }

    /// 面板可访问名。
    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// 层带。
    #[must_use]
    pub fn band(mut self, band: Band) -> Self {
        self.band = band;
        self
    }

    /// 悬停打开子菜单的延迟。
    #[must_use]
    pub fn submenu_delay(mut self, delay: Duration) -> Self {
        self.submenu_delay = delay;
        self
    }
}

// ── 菜单 ──────────────────────────────────────────────────────────────────

/// 菜单激活报告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuActivation {
    /// 条目 id。
    pub id: SmolStr,
}

/// 行的种类（条目下标 + 语义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    Action(usize),
    Submenu(usize),
    Group(usize),
    Separator,
}

/// 激活 / 关闭回调（整条菜单链共享一份）。
#[derive(Default)]
struct MenuSink {
    on_activate: RefCell<Option<Rc<dyn Fn(&mut App, MenuActivation)>>>,
    on_close: RefCell<Option<Rc<dyn Fn(&mut App)>>>,
}

struct OpenSubmenu {
    row: usize,
    menu: Menu,
}

/// 菜单句柄（`Rc` 浅拷贝）。
#[derive(Clone)]
pub struct Menu {
    inner: Rc<MenuInner>,
}

/// 建菜单的公共参数（构造函数参数过多时收成一份）。
struct MenuSeed {
    theme: ThemeHandle,
    layer: Layer,
    options: MenuOptions,
    kind: MenuKind,
    parent: Option<(Weak<MenuInner>, usize)>,
    sink: Rc<MenuSink>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuKind {
    Root,
    Submenu,
}

struct MenuInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: MenuOptions,
    layer: Layer,
    panel: ItemPanel,
    icons: IconCache,
    rows: RefCell<Vec<RowKind>>,
    entries: RefCell<Rc<Vec<MenuEntry>>>,
    kind: MenuKind,
    parent: Option<(Weak<MenuInner>, usize)>,
    sink: Rc<MenuSink>,
    submenus: RefCell<Vec<OpenSubmenu>>,
    submenu_timer: Cell<Option<TimerId>>,
    last_hovered: Cell<Option<usize>>,
    window: Cell<Option<WindowId>>,
    right_action: Action,
    left_action: Action,
    /// 子菜单是否允许声明焦点（键盘打开的层级为 `true`，悬停打开保持父焦点）。
    claim_focus: Cell<bool>,
}

impl Menu {
    /// 建菜单（关闭态）。
    ///
    /// `entries` 可以在之后用 [`Menu::set_entries`] 换一批（面板打开时会重建行）。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        key: impl Into<SmolStr>,
        entries: impl IntoIterator<Item = MenuEntry>,
        options: MenuOptions,
    ) -> Self {
        Self::build(
            app,
            key,
            entries,
            MenuSeed {
                theme,
                layer,
                options,
                kind: MenuKind::Root,
                parent: None,
                sink: Rc::new(MenuSink::default()),
            },
        )
    }

    fn build(
        app: &mut App,
        key: impl Into<SmolStr>,
        entries: impl IntoIterator<Item = MenuEntry>,
        seed: MenuSeed,
    ) -> Self {
        let MenuSeed {
            theme,
            layer,
            options,
            kind,
            parent,
            sink,
        } = seed;
        let key = key.into();
        let root = kind == MenuKind::Root;
        let panel = ItemPanel::new(
            app,
            theme.clone(),
            layer.clone(),
            format_smolstr!("{key}:panel"),
            ItemPanelOptions {
                size: options.size,
                max_height: options.max_height,
                row_height: options.size.h(),
                band: options.band,
                // 子菜单非模态：父面板的键盘保持可用（悬停打开不抢焦点）。
                modal: root,
                placement: options.placement,
                gap: options.gap,
                roles: options.mode.roles(),
                label: options.label.clone(),
            },
        );
        let inner = Rc::new(MenuInner {
            key: key.clone(),
            theme,
            options,
            layer,
            panel,
            icons: IconCache::default(),
            rows: RefCell::new(Vec::new()),
            entries: RefCell::new(Rc::new(entries.into_iter().collect())),
            kind,
            parent,
            sink,
            submenus: RefCell::new(Vec::new()),
            submenu_timer: Cell::new(None),
            last_hovered: Cell::new(None),
            window: Cell::new(None),
            right_action: Action::Custom(format_smolstr!("{key}.right")),
            left_action: Action::Custom(format_smolstr!("{key}.left")),
            claim_focus: Cell::new(false),
        });
        let menu = Self { inner };
        menu.install_content(app);
        menu.install_handlers();
        menu
    }

    /// 菜单键（调试 id / 动作前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 换一批条目（已打开时重建行；关闭子菜单）。
    ///
    /// # Errors
    /// 引擎虚拟列表失效。
    pub fn set_entries(
        &self,
        app: &mut App,
        entries: impl IntoIterator<Item = MenuEntry>,
    ) -> Result<(), EngineError> {
        *self.inner.entries.borrow_mut() = Rc::new(entries.into_iter().collect());
        self.close_submenus(app);
        self.install_content(app);
        Ok(())
    }

    /// 条目快照。
    #[must_use]
    pub fn entries(&self) -> Rc<Vec<MenuEntry>> {
        self.inner.entries.borrow().clone()
    }

    /// 面板打开状态信号。
    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.panel.open_state()
    }

    /// 是否打开。
    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        self.inner.panel.is_open(app)
    }

    /// 浮层面板节点（内容建在这里）。
    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.panel.overlay().panel_node()
    }

    /// surface（键盘作用域）节点。
    #[must_use]
    pub fn surface_node(&self) -> Option<NodeId> {
        self.inner.panel.surface_node()
    }

    /// 行数（含分组 / 分隔线）。
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.inner.rows.borrow().len()
    }

    /// 高亮行。
    #[must_use]
    pub fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.panel.highlighted(app)
    }

    /// 打开菜单（根菜单由调用方给锚点；子菜单由父菜单自己开）。
    ///
    /// # Errors
    /// 浮层参数非法 / 引擎建树失败。
    pub fn open(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
    ) -> Result<bool, OverlayError> {
        self.inner.window.set(Some(window));
        self.resolve_icons(app, window)?;
        let width = self.inner.options.width;
        let opened = self.inner.panel.open(app, window, anchor, width)?;
        if opened {
            self.inner.claim_focus.set(true);
            self.inner.panel.focus(app);
            self.bind_extra_keys(app, window)?;
        }
        Ok(opened)
    }

    /// 打开菜单并让高亮停在指定条目 id 上（右键菜单用）。
    ///
    /// # Errors
    /// 同 [`Menu::open`]。
    pub fn open_at(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        highlighted: Option<&str>,
    ) -> Result<bool, OverlayError> {
        let opened = self.open(app, window, anchor)?;
        if let Some(id) = highlighted
            && let Some(row) = self.row_of(id)
        {
            self.inner.panel.set_highlighted(app, Some(row));
        }
        Ok(opened)
    }

    /// 关闭菜单（先关子菜单；有淡出时由 `sync` 收尾）。
    pub fn close(&self, app: &mut App) -> bool {
        self.close_submenus(app);
        self.inner.panel.close(app)
    }

    /// 立即关闭菜单。
    pub fn close_now(&self, app: &mut App) -> bool {
        self.close_submenus(app);
        self.inner.panel.close_now(app)
    }

    /// 激活回调（条目被点击 / 回车激活；报告后整条菜单链关闭）。
    pub fn on_activate(self, callback: impl Fn(&mut App, MenuActivation) + 'static) -> Self {
        *self.inner.sink.on_activate.borrow_mut() = Some(Rc::new(callback));
        self
    }

    /// 关闭回调（点击外部 / Esc / 选中条目都算；每次打开最多一次）。
    pub fn on_close(self, callback: impl Fn(&mut App) + 'static) -> Self {
        *self.inner.sink.on_close.borrow_mut() = Some(Rc::new(callback));
        self
    }

    /// 帧同步：浮层 / 子菜单 / 悬停开子菜单。
    ///
    /// # Errors
    /// 引擎列表同步错误。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let has_submenu = !self.inner.submenus.borrow().is_empty();
        let claim = self.inner.claim_focus.get() && !has_submenu;
        let mut changed = self.inner.panel.sync(app, claim)?;
        if !self.inner.panel.is_open(app) {
            self.close_submenus(app);
            return Ok(changed);
        }
        changed |= self.sync_hover(app)?;
        let submenus: Vec<Menu> = self
            .inner
            .submenus
            .borrow()
            .iter()
            .map(|open| open.menu.clone())
            .collect();
        for submenu in &submenus {
            changed |= submenu.sync(app)?;
        }
        self.retain_open_submenus(app);
        Ok(changed)
    }

    /// 转发平台事件（面板行点击兜底 + 子菜单）。
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let mut changed = self.inner.panel.dispatch(app, event);
        let submenus: Vec<Menu> = self
            .inner
            .submenus
            .borrow()
            .iter()
            .map(|open| open.menu.clone())
            .collect();
        for submenu in submenus {
            changed |= submenu.dispatch(app, event);
        }
        changed
    }

    /// 释放菜单（关闭 + 摘 handler + 释放信号作用域）。
    pub fn release(&self, app: &mut App) -> usize {
        self.close_now(app);
        if let Some(timer) = self.inner.submenu_timer.take() {
            app.cancel_timer(&timer);
        }
        app.off_action(&self.inner.right_action);
        app.off_action(&self.inner.left_action);
        let mut released = self.inner.panel.release(app);
        let submenus: Vec<Menu> = self
            .inner
            .submenus
            .borrow_mut()
            .drain(..)
            .map(|open| open.menu)
            .collect();
        for submenu in submenus {
            released += submenu.release(app);
        }
        released
    }

    // ── 内部 ──

    /// 把条目解释成行 + 建行内容 / a11y 回调，写进面板。
    fn install_content(&self, app: &mut App) {
        let entries = self.inner.entries.borrow().clone();
        let mut rows = Vec::with_capacity(entries.len());
        let mut kinds = Vec::with_capacity(entries.len());
        let size = self.inner.options.size;
        let mut has_icon = false;
        for (index, entry) in entries.iter().enumerate() {
            match entry {
                MenuEntry::Action(action) => {
                    has_icon |= action.icon.is_some();
                    rows.push(PanelRow {
                        height: size.h(),
                        disabled: action.disabled,
                    });
                    kinds.push(RowKind::Action(index));
                }
                MenuEntry::Submenu(submenu) => {
                    has_icon |= submenu.icon.is_some();
                    rows.push(PanelRow {
                        height: size.h(),
                        disabled: submenu.disabled,
                    });
                    kinds.push(RowKind::Submenu(index));
                }
                MenuEntry::Group(_) => {
                    rows.push(PanelRow {
                        height: size.h() * GROUP_HEIGHT_RATIO,
                        disabled: true,
                    });
                    kinds.push(RowKind::Group(index));
                }
                MenuEntry::Separator => {
                    rows.push(PanelRow {
                        height: SEPARATOR_HEIGHT,
                        disabled: true,
                    });
                    kinds.push(RowKind::Separator);
                }
            }
        }
        *self.inner.rows.borrow_mut() = kinds;
        let theme = self.inner.theme.clone();
        let icons = Rc::new(self.icon_snapshot());
        let entries_build = entries.clone();
        let build: Rc<dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>> =
            Rc::new(move |tree, parent, index| {
                let Some(entry) = entries_build.get(index) else {
                    return Ok(());
                };
                let text_style = TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap);
                match entry {
                    MenuEntry::Action(action) => {
                        build_icon_slot(tree, parent, &theme, &icons, action.icon, size, has_icon)?;
                        build_label(
                            tree,
                            parent,
                            &theme,
                            &action.label,
                            text_style,
                            action_foreground(action),
                        )?;
                        if let Some(shortcut) = &action.shortcut {
                            build_shortcut(tree, parent, &theme, shortcut, size)?;
                        }
                        if let Some(checked) = action.checked {
                            build_check(tree, parent, &theme, checked, size)?;
                        }
                        Ok(())
                    }
                    MenuEntry::Submenu(submenu) => {
                        build_icon_slot(
                            tree,
                            parent,
                            &theme,
                            &icons,
                            submenu.icon,
                            size,
                            has_icon,
                        )?;
                        let foreground = if submenu.disabled {
                            Role::FgDisabled
                        } else {
                            Role::FgDefault
                        };
                        build_label(tree, parent, &theme, &submenu.label, text_style, foreground)?;
                        if let Some(resource) = icons.get(IconKind::CaretRight, size.icon_size()) {
                            let theme_icon = theme.clone();
                            build_icon(tree, parent, resource, size.icon_size(), move |cx| {
                                StylePatch {
                                    foreground: Some(theme_icon.role(cx, Role::FgMuted)),
                                    ..StylePatch::default()
                                }
                            })?;
                        }
                        Ok(())
                    }
                    MenuEntry::Group(label) => {
                        build_label(tree, parent, &theme, label, text_style, Role::FgMuted)
                    }
                    MenuEntry::Separator => {
                        let theme_line = theme.clone();
                        NodeBuilder::boxed()
                            .style(StyleBuilder::new().width_full().height(1.0).build())
                            .bind_style(move |cx| StylePatch {
                                background: Some(theme_line.role(cx, Role::LineDivider)),
                                ..StylePatch::default()
                            })
                            .a11y(A11yOverride {
                                hidden: true,
                                ..A11yOverride::default()
                            })
                            .build(tree, parent)
                            .map(|_| ())
                    }
                }
            });
        let roles = self.inner.options.mode.roles();
        let entries_a11y = entries;
        let a11y: Rc<dyn Fn(usize) -> A11yPatch> = Rc::new(move |index| {
            let Some(entry) = entries_a11y.get(index) else {
                return A11yPatch {
                    hidden: Some(true),
                    ..A11yPatch::default()
                };
            };
            match entry {
                MenuEntry::Action(action) => A11yPatch {
                    role: Some(roles.row),
                    label: Some(action.label.clone()),
                    disabled: Some(action.disabled),
                    checked: action.checked,
                    selected: match (roles.row, action.checked) {
                        (A11yRole::ListBoxOption, Some(checked)) => Some(checked),
                        _ => None,
                    },
                    ..A11yPatch::default()
                },
                MenuEntry::Submenu(submenu) => A11yPatch {
                    role: Some(roles.row),
                    label: Some(submenu.label.clone()),
                    disabled: Some(submenu.disabled),
                    ..A11yPatch::default()
                },
                MenuEntry::Group(label) => A11yPatch {
                    role: Some(A11yRole::Label),
                    label: Some(label.clone()),
                    ..A11yPatch::default()
                },
                MenuEntry::Separator => A11yPatch {
                    role: Some(A11yRole::Separator),
                    hidden: Some(true),
                    ..A11yPatch::default()
                },
            }
        });
        let _ = self
            .inner
            .panel
            .set_content(app, PanelContent { rows, build, a11y });
    }

    /// 面板激活 / 关闭回调。
    fn install_handlers(&self) {
        let this = self.clone();
        self.inner.panel.on_activate(move |app, row| {
            this.handle_row(app, row);
        });
        let this = self.clone();
        self.inner.panel.on_close(move |app| {
            this.close_submenus(app);
            this.fire_close(app);
        });
    }

    /// 行被激活（点击 / 回车）：子菜单开子层，动作条目上报 + 关链。
    fn handle_row(&self, app: &mut App, row: usize) {
        let kind = self.inner.rows.borrow().get(row).copied();
        match kind {
            Some(RowKind::Submenu(index)) => {
                self.open_submenu(app, row, index, true);
            }
            Some(RowKind::Action(index)) => {
                let entries = self.inner.entries.borrow().clone();
                let Some(MenuEntry::Action(action)) = entries.get(index) else {
                    return;
                };
                if action.disabled {
                    return;
                }
                let activation = MenuActivation {
                    id: action.id.clone(),
                };
                if let Some(callback) = self.inner.sink.on_activate.borrow().clone() {
                    callback(app, activation);
                }
                self.close_chain(app);
            }
            Some(RowKind::Group(_) | RowKind::Separator) | None => {}
        }
    }

    fn fire_close(&self, app: &mut App) {
        if self.inner.kind != MenuKind::Root {
            return;
        }
        if let Some(callback) = self.inner.sink.on_close.borrow().clone() {
            callback(app);
        }
    }

    /// 关闭本层并向上逐层关闭（选中条目 / `←` 的收尾语义）。
    fn close_chain(&self, app: &mut App) {
        self.inner.panel.close_now(app);
        let parent = self
            .inner
            .parent
            .as_ref()
            .and_then(|(parent, _)| parent.upgrade());
        if let Some(parent) = parent {
            Menu { inner: parent }.close_chain(app);
        }
    }

    /// 打开子菜单（`row` 面板行号 / `index` 条目下标）。
    fn open_submenu(&self, app: &mut App, row: usize, index: usize, focus: bool) {
        if self
            .inner
            .submenus
            .borrow()
            .iter()
            .any(|open| open.row == row && open.menu.is_open(app))
        {
            return;
        }
        let entries = self.inner.entries.borrow().clone();
        let Some(MenuEntry::Submenu(submenu)) = entries.get(index) else {
            return;
        };
        if submenu.disabled {
            return;
        }
        let (Some(window), Some(row_node)) = (
            self.inner.window.get(),
            self.inner.panel.list().row_node(app, row),
        ) else {
            return;
        };
        self.close_submenus(app);
        let mut options = self.inner.options.clone();
        options.placement = Placement::RIGHT_START;
        options.gap = SUBMENU_GAP;
        options.band = Band::Popup;
        let child = Menu::build(
            app,
            format_smolstr!("{}:sub:{row}", self.inner.key),
            submenu.entries.clone(),
            MenuSeed {
                theme: self.inner.theme.clone(),
                layer: self.inner.layer.clone(),
                options,
                kind: MenuKind::Submenu,
                parent: Some((Rc::downgrade(&self.inner), row)),
                sink: self.inner.sink.clone(),
            },
        );
        child.inner.claim_focus.set(focus);
        let anchor = Anchor::node(row_node);
        match child.open(app, window, anchor) {
            Ok(true) => {
                if focus {
                    let _ = child.inner.panel.focus(app);
                }
                self.inner
                    .submenus
                    .borrow_mut()
                    .push(OpenSubmenu { row, menu: child });
            }
            Ok(false) => {
                let _ = child.release(app);
            }
            Err(_) => {
                let _ = child.release(app);
            }
        }
    }

    /// 关闭一层子菜单（先关更深层）。
    fn close_submenus(&self, app: &mut App) {
        let submenus: Vec<OpenSubmenu> = self.inner.submenus.borrow_mut().drain(..).collect();
        for open in submenus.into_iter().rev() {
            open.menu.close_now(app);
            let _ = open.menu.release(app);
        }
        if let Some(timer) = self.inner.submenu_timer.take() {
            app.cancel_timer(&timer);
        }
        self.inner.last_hovered.set(None);
    }

    /// 子菜单被引擎关闭（`Esc` / 点击外部）后从表里摘掉。
    fn retain_open_submenus(&self, app: &mut App) {
        let mut stale: Vec<OpenSubmenu> = Vec::new();
        {
            let mut submenus = self.inner.submenus.borrow_mut();
            let mut index = 0;
            while index < submenus.len() {
                if submenus[index].menu.is_open(app) {
                    index += 1;
                } else {
                    stale.push(submenus.remove(index));
                }
            }
        }
        for open in stale {
            let _ = open.menu.release(app);
        }
    }

    /// 悬停行变化 ⇒ 延迟打开子菜单 / 关闭当前子菜单。
    fn sync_hover(&self, app: &mut App) -> Result<bool, EngineError> {
        let Some(window) = self.inner.window.get() else {
            return Ok(false);
        };
        let hovered = self.inner.panel.hovered(app);
        if hovered == self.inner.last_hovered.get() {
            return Ok(false);
        }
        self.inner.last_hovered.set(hovered);
        if let Some(timer) = self.inner.submenu_timer.take() {
            app.cancel_timer(&timer);
        }
        let kind = hovered.and_then(|row| {
            self.inner
                .rows
                .borrow()
                .get(row)
                .copied()
                .map(|kind| (row, kind))
        });
        match kind {
            Some((row, RowKind::Submenu(index))) => {
                let delay = self.inner.options.submenu_delay;
                if delay.is_zero() {
                    self.open_submenu(app, row, index, false);
                } else {
                    let this = self.clone();
                    let timer = app.after(window, delay, move |app| {
                        if this.inner.last_hovered.get() == Some(row) {
                            this.open_submenu(app, row, index, false);
                        }
                    })?;
                    self.inner.submenu_timer.set(Some(timer));
                }
            }
            Some((_, RowKind::Action(_) | RowKind::Group(_) | RowKind::Separator)) => {
                self.close_submenus(app);
            }
            None => {}
        }
        Ok(true)
    }

    /// `→` 打开 / 进入子菜单，`←` 关闭本层。
    fn bind_extra_keys(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let Some(surface) = self.inner.panel.surface_node() else {
            return Ok(());
        };
        for (key, action) in [
            (KeyCode::ArrowRight, self.inner.right_action.clone()),
            (KeyCode::ArrowLeft, self.inner.left_action.clone()),
        ] {
            app.bind_key_scoped(
                window,
                surface,
                KeyBinding {
                    key,
                    modifiers: Modifiers::empty(),
                    action,
                },
            )?;
        }
        let this = self.clone();
        app.on_action(self.inner.right_action.clone(), move |ctx| {
            if let Some(row) = this.inner.panel.highlighted(ctx.app_mut())
                && let Some(RowKind::Submenu(index)) = this.inner.rows.borrow().get(row).copied()
            {
                let app = ctx.app_mut();
                this.open_submenu(app, row, index, true);
            }
        });
        let this = self.clone();
        app.on_action(self.inner.left_action.clone(), move |ctx| {
            if this.inner.kind != MenuKind::Submenu {
                return;
            }
            let app = ctx.app_mut();
            this.close_now(app);
            let parent = this
                .inner
                .parent
                .as_ref()
                .and_then(|(parent, row)| parent.upgrade().map(|parent| (parent, *row)));
            if let Some((parent, row)) = parent {
                let parent = Menu { inner: parent };
                parent.inner.panel.set_highlighted(app, Some(row));
                let _ = parent.inner.panel.focus(app);
            }
        });
        Ok(())
    }

    /// 图标资源表（行内容建树时读）。
    fn icon_snapshot(&self) -> IconSnapshot {
        IconSnapshot {
            entries: self
                .inner
                .icons
                .entries
                .borrow()
                .iter()
                .map(|entry| (entry.kind, entry.size, entry.resource))
                .collect(),
        }
    }

    fn resolve_icons(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let entries = self.inner.entries.borrow().clone();
        let size = self.inner.options.size;
        for entry in entries.iter() {
            let icon = match entry {
                MenuEntry::Action(action) => action.icon,
                MenuEntry::Submenu(submenu) => submenu.icon,
                MenuEntry::Group(_) | MenuEntry::Separator => None,
            };
            if let Some(kind) = icon {
                self.inner
                    .icons
                    .resolve(app, window, kind, size.icon_size())?;
            }
        }
        if self.inner.options.mode == MenuMode::Menu
            && entries
                .iter()
                .any(|entry| matches!(entry, MenuEntry::Submenu(_)))
        {
            self.inner
                .icons
                .resolve(app, window, IconKind::CaretRight, size.icon_size())?;
        }
        Ok(())
    }

    /// 条目 id → 面板行号。
    fn row_of(&self, id: &str) -> Option<usize> {
        let entries = self.inner.entries.borrow().clone();
        let rows = self.inner.rows.borrow();
        rows.iter().enumerate().find_map(|(row, kind)| {
            let index = match kind {
                RowKind::Action(index) | RowKind::Submenu(index) | RowKind::Group(index) => *index,
                RowKind::Separator => return None,
            };
            match entries.get(index) {
                Some(MenuEntry::Action(action)) if action.id == id => Some(row),
                _ => None,
            }
        })
    }
}

/// 图标资源快照（行内容建树时读；`IconCache` 的只读视图）。
#[derive(Clone, Default)]
struct IconSnapshot {
    entries: Vec<(IconKind, f32, ResourceId)>,
}

impl IconSnapshot {
    fn get(&self, kind: IconKind, size: f32) -> Option<ResourceId> {
        self.entries
            .iter()
            .find(|(entry_kind, entry_size, _)| *entry_kind == kind && *entry_size == size)
            .map(|(_, _, resource)| *resource)
    }
}

/// 图标槽（有图标则画，没有也画占位，让标签左对齐）。
fn build_icon_slot(
    tree: &mut NodeTree,
    parent: NodeId,
    theme: &ThemeHandle,
    icons: &IconSnapshot,
    icon: Option<IconKind>,
    size: UnifiedSize,
    reserve: bool,
) -> Result<(), EngineError> {
    let side = size.icon_size();
    match icons.get(icon.unwrap_or(IconKind::X), side) {
        Some(resource) if icon.is_some() => {
            let theme = theme.clone();
            build_icon(tree, parent, resource, side, move |cx| StylePatch {
                foreground: Some(theme.role(cx, Role::FgMuted)),
                ..StylePatch::default()
            })
        }
        _ if reserve => NodeBuilder::boxed()
            .style(StyleBuilder::new().size(side, side).build())
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, parent)
            .map(|_| ()),
        _ => Ok(()),
    }
}

/// 条目文本（`flex_1` + 省略号，压不进面板宽度时截断）。
fn build_label(
    tree: &mut NodeTree,
    parent: NodeId,
    theme: &ThemeHandle,
    label: &str,
    text_style: TextStyle,
    foreground: Role,
) -> Result<(), EngineError> {
    let theme = theme.clone();
    NodeBuilder::text(label, text_style)
        .style(StyleBuilder::new().flex_1().min_width_0().build())
        .bind_style(move |cx| StylePatch {
            foreground: Some(theme.role(cx, foreground)),
            ..StylePatch::default()
        })
        .build(tree, parent)
        .map(|_| ())
}

/// 快捷键提示（右对齐、弱化）。
fn build_shortcut(
    tree: &mut NodeTree,
    parent: NodeId,
    theme: &ThemeHandle,
    shortcut: &str,
    size: UnifiedSize,
) -> Result<(), EngineError> {
    let theme = theme.clone();
    NodeBuilder::text(
        shortcut,
        TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
    )
    .style(StyleBuilder::new().flex_none().build())
    .bind_style(move |cx| StylePatch {
        foreground: Some(theme.role(cx, Role::FgMuted)),
        ..StylePatch::default()
    })
    .build(tree, parent)
    .map(|_| ())
}

/// 勾选标记（勾选型条目）。
fn build_check(
    tree: &mut NodeTree,
    parent: NodeId,
    theme: &ThemeHandle,
    checked: bool,
    size: UnifiedSize,
) -> Result<(), EngineError> {
    let theme = theme.clone();
    NodeBuilder::text(
        if checked { "✓" } else { " " },
        TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
    )
    .style(StyleBuilder::new().flex_none().build())
    .bind_style(move |cx| StylePatch {
        foreground: Some(theme.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree, parent)
    .map(|_| ())
}

/// 条目前景角色（危险 / 禁用 / 常规）。
fn action_foreground(action: &MenuAction) -> Role {
    if action.disabled {
        Role::FgDisabled
    } else if action.danger {
        Role::StatusDangerBase
    } else {
        Role::FgDefault
    }
}

// `ActionCtx` 在动作 handler 里用到（`register_actions` / `bind_extra_keys` 的闭包签名）。
const _: fn(&mut ActionCtx<'_>) = |_| {};

#[cfg(test)]
mod tests {
    //! 引擎树上的行为：打开 / 定位翻边 / 键盘高亮 / 激活 / 关闭语义 / 子菜单 / 虚拟化 / a11y / I3。

    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::Duration,
    };

    use smol_str::SmolStr;
    use ui_core::{
        a11y::A11yRole,
        event::{InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Rect, Size},
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{
        Menu, MenuAction, MenuEntry, MenuMode, MenuOptions, MenuSubmenu, Overlay, Placement,
    };
    use crate::{
        kit::{layer::Layer, overlay::Anchor, test_support::full_a11y},
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    /// 锚点按钮的几何（窗口中部，上下都有空间）。
    const ANCHOR: Rect = Rect::new(300.0, 200.0, 120.0, 32.0);

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
        layer: Layer,
        anchor: NodeId,
        activated: Rc<RefCell<Vec<SmolStr>>>,
        closes: Rc<Cell<usize>>,
    }

    impl Harness {
        fn new() -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let anchor = NodeBuilder::boxed()
                .id("anchor")
                .layout_override(ANCHOR)
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("锚点");
            Self {
                app,
                window,
                theme,
                layer: Layer::new(),
                anchor,
                activated: Rc::new(RefCell::new(Vec::new())),
                closes: Rc::new(Cell::new(0)),
            }
        }

        fn menu(&mut self, entries: Vec<MenuEntry>, options: MenuOptions) -> Menu {
            let activated = self.activated.clone();
            let closes = self.closes.clone();
            Menu::new(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                "menu",
                entries,
                options,
            )
            .on_activate(move |_, activation| activated.borrow_mut().push(activation.id))
            .on_close(move |_| closes.set(closes.get() + 1))
        }

        fn open(&mut self, menu: &Menu) -> bool {
            let opened = menu
                .open(self.app.app_mut(), self.window, Anchor::node(self.anchor))
                .expect("打开菜单");
            self.settle(menu);
            opened
        }

        fn settle(&mut self, menu: &Menu) {
            for _ in 0..6 {
                self.app.advance(Duration::from_millis(40));
                let _ = self.app.drain().expect("帧");
                let _ = menu.sync(self.app.app_mut());
            }
            self.app.advance(Duration::from_millis(200));
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
        }

        fn key(&mut self, menu: &Menu, code: KeyCode) {
            let _ = self
                .app
                .inject(InputEvent::key_down(code, Modifiers::empty()));
            let _ = self
                .app
                .inject(InputEvent::key_up(code, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
        }

        fn click(&mut self, menu: &Menu, at: ui_core::math::Point) {
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
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
        }

        fn hover(&mut self, menu: &Menu, at: ui_core::math::Point) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(at, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
        }

        fn advance(&mut self, menu: &Menu, duration: Duration) {
            self.app.advance(duration);
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
        }

        fn bounds(&self, node: NodeId) -> Rect {
            self.app.probe().bounds(node).expect("布局盒")
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
            self.find(&format!("menu:panel:list:row:{index}"))
                .unwrap_or_else(|| panic!("第 {index} 行未挂载"))
        }

        fn row_center(&self, index: usize) -> ui_core::math::Point {
            let bounds = self.bounds(self.row(index));
            ui_core::math::Point::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        /// 根的子节点数（浮层是 root 的子节点：菜单 / 子菜单各一个）。
        fn root_children(&self) -> Vec<NodeId> {
            self.app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .get(
                    self.app
                        .app()
                        .window(self.window)
                        .expect("窗口")
                        .tree()
                        .root(),
                )
                .expect("根")
                .children()
                .to_vec()
        }

        fn idles(&mut self) -> bool {
            let record = self.app.drain().expect("帧");
            !record.rendered
        }
    }

    fn entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Group(SmolStr::new_static("账户")),
            MenuEntry::Action(MenuAction::new("profile", "资料")),
            MenuEntry::Action(MenuAction::new("blocked", "已禁用").disabled(true)),
            MenuEntry::Separator,
            MenuEntry::Action(
                MenuAction::new("delete", "删除")
                    .danger(true)
                    .shortcut("Del"),
            ),
        ]
    }

    fn options() -> MenuOptions {
        MenuOptions::default()
    }

    #[test]
    fn the_menu_opens_below_its_anchor_at_its_stated_width() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        assert!(harness.open(&menu));
        assert!(menu.is_open(harness.app.app()));

        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert_eq!(
            panel.origin.x, ANCHOR.origin.x,
            "左对齐锚点（BOTTOM_START）"
        );
        assert!(panel.origin.y >= ANCHOR.max().y, "面板在锚点下方");
        assert_eq!(panel.size.width, 200.0, "面板宽度 = MenuOptions.width");
    }

    #[test]
    fn a_menu_near_the_bottom_flips_above_its_anchor() {
        let mut harness = Harness::new();
        // 把锚点挪到窗口底部（TestApp 默认 800×600）。
        let root = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .root();
        let bottom = NodeBuilder::boxed()
            .id("bottom-anchor")
            .layout_override(Rect::new(300.0, 560.0, 120.0, 32.0))
            .build(
                harness
                    .app
                    .app_mut()
                    .window_mut(harness.window)
                    .expect("窗口")
                    .tree_mut(),
                root,
            )
            .expect("锚点");
        let menu = harness.menu(entries(), options());
        let opened = menu
            .open(harness.app.app_mut(), harness.window, Anchor::node(bottom))
            .expect("打开");
        assert!(opened);
        harness.settle(&menu);
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            panel.max().y <= 560.0 + 0.5,
            "放不下时翻到锚点上方：{panel:?}"
        );
        let _ = Size::new(800.0, 600.0);
    }

    #[test]
    fn arrow_keys_move_the_highlight_and_skip_non_actionable_rows() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1), "首个可激活行");

        harness.key(&menu, KeyCode::ArrowDown);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(4),
            "跳过禁用行与分隔线"
        );
        harness.key(&menu, KeyCode::ArrowDown);
        assert_eq!(menu.highlighted(harness.app.app()), Some(4), "末行不环绕");
        harness.key(&menu, KeyCode::ArrowUp);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1));
    }

    #[test]
    fn home_and_end_jump_to_the_first_and_last_actionable_rows() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::End);
        assert_eq!(menu.highlighted(harness.app.app()), Some(4));
        harness.key(&menu, KeyCode::Home);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1));
    }

    #[test]
    fn enter_activates_the_highlighted_entry_and_closes_the_menu() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::End);
        harness.key(&menu, KeyCode::Enter);

        assert_eq!(
            harness.activated.borrow().as_slice(),
            &[SmolStr::new_static("delete")],
            "回车激活高亮条目"
        );
        assert!(!menu.is_open(harness.app.app()), "选中后关闭菜单");
        assert_eq!(harness.closes.get(), 1, "关闭回调一次");
    }

    #[test]
    fn a_pointer_press_activates_the_entry_it_lands_on() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        let at = harness.row_center(1);
        harness.click(&menu, at);

        assert_eq!(
            harness.activated.borrow().as_slice(),
            &[SmolStr::new_static("profile")],
            "点击行激活该条目"
        );
        assert!(!menu.is_open(harness.app.app()));
    }

    #[test]
    fn a_disabled_entry_is_not_activated() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        let at = harness.row_center(2);
        harness.click(&menu, at);
        assert!(harness.activated.borrow().is_empty(), "禁用条目不得激活");
        assert!(menu.is_open(harness.app.app()), "禁用条目不关菜单");
    }

    #[test]
    fn escape_closes_the_menu_and_reports_it() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()), "Esc 关闭最上层浮层");
        assert_eq!(harness.closes.get(), 1);
    }

    #[test]
    fn a_press_outside_closes_the_menu() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.click(&menu, ui_core::math::Point::new(10.0, 10.0));
        assert!(!menu.is_open(harness.app.app()), "点击外部关闭");
        assert_eq!(harness.closes.get(), 1);
    }

    #[test]
    fn a_submenu_opens_beside_its_row_after_the_hover_delay() {
        let mut harness = Harness::new();
        let entries = vec![
            MenuEntry::Action(MenuAction::new("a", "A")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [MenuEntry::Action(MenuAction::new("inner", "内部"))],
            )),
        ];
        let menu = harness.menu(entries, options());
        harness.open(&menu);
        let root_before = harness.root_children().len();

        let at = harness.row_center(1);
        harness.hover(&menu, at);
        let root_during = harness.root_children().len();
        assert_eq!(root_during, root_before, "延迟未到不开子菜单");

        harness.advance(&menu, Duration::from_millis(200));
        let root_after = harness.root_children();
        assert_eq!(root_after.len(), root_before + 1, "延迟到点打开子菜单");
        let submenu_overlay = *root_after.last().expect("子菜单浮层");
        let submenu_panel = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .get(submenu_overlay)
            .expect("子菜单浮层")
            .children()
            .first()
            .copied()
            .expect("子菜单面板");
        let submenu = harness.bounds(submenu_panel);
        let row_bounds = harness.bounds(harness.row(1));
        assert!(
            submenu.origin.x >= row_bounds.max().x - 0.5,
            "子菜单在所属行右侧：行 {row_bounds:?} / 子 {submenu:?}"
        );
        assert!(
            (submenu.origin.y - row_bounds.origin.y).abs() < 1.0,
            "子菜单与所属行顶部对齐"
        );
    }

    #[test]
    fn escape_closes_the_submenu_before_the_menu_it_belongs_to() {
        let mut harness = Harness::new();
        let entries = vec![
            MenuEntry::Action(MenuAction::new("a", "A")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [MenuEntry::Action(MenuAction::new("inner", "内部"))],
            )),
        ];
        let menu = harness.menu(entries, options());
        harness.open(&menu);
        let root_before = harness.root_children().len();
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(
            harness.root_children().len(),
            root_before + 1,
            "→ 打开子菜单"
        );

        harness.key(&menu, KeyCode::Escape);
        assert!(menu.is_open(harness.app.app()), "第一下 Esc 只关子菜单");
        assert_eq!(harness.root_children().len(), root_before);

        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()), "第二下 Esc 关父菜单");
        assert_eq!(harness.closes.get(), 1);
    }

    #[test]
    fn a_long_menu_virtualizes_its_rows_and_scrolls_to_the_end() {
        let mut harness = Harness::new();
        let entries: Vec<MenuEntry> = (0..80)
            .map(|index| {
                let id = format!("entry-{index}");
                MenuEntry::Action(MenuAction::new(id.clone(), id))
            })
            .collect();
        let menu = harness.menu(
            entries,
            MenuOptions {
                max_height: 200.0,
                ..options()
            },
        );
        harness.open(&menu);
        assert_eq!(menu.row_count(), 80);
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            panel.size.height <= 200.0 + 0.5,
            "面板高度被 max_height 截住：{panel:?}"
        );
        let mounted = count_rows(&harness);
        assert!(
            mounted < 20,
            "长菜单只挂可见窗口内的行（实际 {mounted} / 80）"
        );

        harness.key(&menu, KeyCode::End);
        assert_eq!(menu.highlighted(harness.app.app()), Some(79));
        assert!(
            harness.find("menu:panel:list:row:79").is_some(),
            "末行已挂载"
        );
    }

    #[test]
    fn the_menu_declares_menu_and_menu_item_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        let snapshot = full_a11y(&mut harness.app);

        let panel = menu.panel_node().expect("面板");
        let panel_record = snapshot.node_for(panel).expect("面板在 a11y 树里");
        assert_eq!(panel_record.role, A11yRole::Menu);

        let row = {
            let mut found = None;
            for node in snapshot.nodes() {
                if node.role == A11yRole::MenuItem {
                    found = Some(node);
                    break;
                }
            }
            found.expect("行声明 MenuItem")
        };
        let label = row
            .label
            .as_ref()
            .map(|label| label.text().to_owned())
            .unwrap_or_default();
        assert_eq!(label, "资料", "MenuItem 的可访问名是条目文本");

        let row_for_blocked = snapshot
            .nodes()
            .iter()
            .find(|node| {
                node.role == A11yRole::MenuItem
                    && node
                        .label
                        .as_ref()
                        .is_some_and(|label| label.text() == "已禁用")
            })
            .expect("禁用条目也在 a11y 树里");
        assert!(row_for_blocked.disabled, "禁用态声明到 a11y");
    }

    #[test]
    fn a_settled_menu_with_no_input_requests_no_frames() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        assert!(
            !menu.sync(harness.app.app_mut()).expect("同步"),
            "稳态无工作"
        );
        assert!(harness.idles(), "稳态不出帧（I3）");
    }

    #[test]
    fn a_menu_opened_from_a_modal_window_keeps_the_scope_it_was_opened_in() {
        // E12：菜单是模态浮层，背景节点在它开着时不接收悬停。
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        assert!(
            harness.app.app().modal_scope(harness.window).is_some(),
            "菜单自带模态作用域"
        );
        harness.app.inject(InputEvent::pointer_move(
            ui_core::math::Point::new(5.0, 5.0),
            Modifiers::empty(),
        ));
        let _ = harness.app.drain().expect("帧");
        assert_eq!(
            harness.app.app().hovered(harness.window),
            None,
            "模态下的背景悬停被过滤（E12）"
        );
    }

    fn count_rows(harness: &Harness) -> usize {
        let tree = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree();
        let mut stack = vec![tree.root()];
        let mut rows = 0;
        while let Some(node) = stack.pop() {
            if let Some(id) = tree.debug_id(node)
                && id.starts_with("menu:panel:list:row:")
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

    /// `MenuMode::ListBox` 的 a11y 角色对（选择器面板；本模块只验证角色映射）。
    #[test]
    fn the_list_box_mode_declares_list_and_list_box_option_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(
            entries(),
            MenuOptions {
                mode: MenuMode::ListBox,
                ..options()
            },
        );
        harness.open(&menu);
        let snapshot = full_a11y(&mut harness.app);
        let panel = menu.panel_node().expect("面板");
        assert_eq!(snapshot.node_for(panel).expect("面板").role, A11yRole::List);
        assert!(
            snapshot
                .nodes()
                .iter()
                .any(|node| node.role == A11yRole::ListBoxOption),
            "行声明 ListBoxOption"
        );
    }

    /// 未使用的类型约束：`Overlay` / `Placement` 由本模块文档引用（避免 import 落空）。
    const _: Option<fn(&Overlay, Placement)> = None;
}

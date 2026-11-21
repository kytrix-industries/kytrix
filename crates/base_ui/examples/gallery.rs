//! `base_ui::kit` 组件的**真实窗口画廊**（P2-5 验收示例）。
//!
//! ```text
//! cargo run -p base_ui --example gallery
//! cargo run -p base_ui --example gallery -- --auto-demo --idle-check
//! ```
//!
//! 单文件、只依赖 `ui_core` + `base_ui::{kit, theme, style_builder}`：
//!
//! ```text
//! 窗口（1200×800，系统标题栏）
//! ├─ 顶栏：标题 / 副标题 / DPI 与外观状态 / 深浅切换按钮
//! └─ 滚动视口（引擎 scroll 容器，滚轮滚动）
//!    └─ 内容列（flow flex_col，15 个分区卡片）
//!       ├─ ① 主题色卡 / 图标 ×55 / 品牌 ×7
//!       ├─ ② 按钮（5 变体 × 3 尺寸 + 禁用 / 加载 / 图标 / 选中）
//!       ├─ ③ 开关 / 复选 / 单选（Toggle）
//!       ├─ ④ 标签 / 进度 / 加载 / 分割线 / 空态
//!       ├─ ⑤ 标签栏（TabBar）
//!       ├─ ⑥ 列表（等高 / 变高 / 双端 deque）
//!       ├─ ⑦ 自绘滚动条（独立 ScrollBar + 引擎 scroll 容器）
//!       ├─ ⑧ 表格（跨列 / 排序 / 多选 / 变高）
//!       ├─ ⑨ 树（展开折叠 / 键盘 / 选择）
//!       ├─ ⑩ 输入框（单行 / 掩码 / 禁用 / 多行 + IME）
//!       ├─ ⑪ markdown（富文本 / 代码 / 表格 / 行内图片占位）
//!       ├─ ⑫ 浮层（Overlay / Tooltip / Dropdown / Select / Menu / ContextMenu）
//!       ├─ ⑬ 对话框 / Alert（Dialog）
//!       ├─ ⑭ 颜色选择器（ColorPicker）
//!       └─ ⑮ 交互日志（最近 8 条事件）
//! ```
//!
//! # 验收钩子（`--` 之后的参数）
//!
//! - `--auto-demo`：1.2 s 后经 `App::dispatch_action(.., ActionSource::Programmatic)` 触发主按钮，
//!   打印派发前后的帧号并写一条交互日志（没有 GUI 注入能力的环境用它证明动作路径可用）；
//! - `--idle-check`：1 s 取基线帧号，再过 3 s 比较：空闲帧增量 ≤ 2 打印 PASS（I3），然后关窗；
//! - `--auto-close=<秒>`：到点关闭窗口（无人值守验收用；默认不自动关）。
//!
//! 退出码：`0` = 正常关窗（或 idle-check PASS）；`3` = idle-check FAIL。
//!
//! 本示例真实窗口运行中暴露的 kit / 引擎缺口记录在
//! `.docs/gui-migration/spec-deltas/kit-gallery.md`。

use std::{borrow::Cow, cell::RefCell, rc::Rc, sync::Arc, time::Duration};

use base_ui::{
    kit::{
        alert::AlertBuilder,
        badge::Badge,
        brand::{Brand, BrandKind},
        button::{Button, ButtonVariant},
        color_picker::ColorPicker,
        context_menu::{ContextMenu, ContextMenuOptions},
        dialog::{Dialog, DialogBuilder},
        dropdown::{Dropdown, DropdownItem, DropdownOptions},
        empty_state::EmptyState,
        icon::{IconKind, IconSpec},
        input::{self, InputNode, InputSpec, InputState},
        layer::{Band, Layer},
        list::{List, ListOptions, SelectionMode},
        markdown::{parse::parse_document, source::MarkdownAssets, view::MarkdownView},
        menu::{Menu, MenuAction, MenuEntry, MenuOptions, MenuSubmenu},
        overlay::{Anchor, AnimationRouter, Overlay, OverlayOptions},
        progress::Progress,
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
        select::{Select, SelectItem, SelectOptions},
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
        spinner::Spinner,
        tab_bar::{IndicatorAnimation, TabBar, TabBarOptions, TabItem},
        table::{Column, ColumnAlign, Table, TableOptions},
        toggle::Toggle,
        tooltip::{Tooltip, TooltipContent, TooltipOptions},
        tree::{Tree, TreeItem, TreeOptions},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{appearance::Appearance, components::ControlSize, handle::ThemeHandle, role::Role},
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::{App, Application},
    event::{Action, ActionSource, Event, InputEvent},
    interaction::ScrollConfig,
    math::{Point, Rect, Size},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{
        color::Color,
        primitive::Border,
        style::{PaintSpec, Style, StylePatch},
    },
    text::{TextStyle, TextWrap},
    window::{WindowId, WindowOptions},
};

// ─────────────────────────────────────────────────────────────────────────────
// 常量
// ─────────────────────────────────────────────────────────────────────────────

/// 窗口逻辑宽度。
const WINDOW_W: f32 = 1200.0;
/// 窗口逻辑高度。
const WINDOW_H: f32 = 800.0;
/// 顶栏高度。
const HEADER_H: f32 = 62.0;
/// 内容列左右外边距。
const SIDE_MARGIN: f32 = 20.0;
/// 分区卡片宽度。
const SECTION_W: f32 = WINDOW_W - SIDE_MARGIN * 2.0;
/// 内容列顶部内边距。
const CONTENT_TOP: f32 = 16.0;
/// 内容列底部内边距。
const CONTENT_BOTTOM: f32 = 20.0;
/// 分区之间的垂直间距。
const SECTION_GAP: f32 = 12.0;
/// 日志最多保留多少条。
const LOG_CAPACITY: usize = 8;

/// ① 主题 / 图标 / 品牌。
const H_THEME: f32 = 250.0;
/// ② 按钮。
const H_BUTTONS: f32 = 210.0;
/// ③ 开关。
const H_TOGGLES: f32 = 200.0;
/// ④ 反馈控件。
const H_FEEDBACK: f32 = 210.0;
/// ⑤ 标签栏。
const H_TABS: f32 = 130.0;
/// ⑥ 列表。
const H_LISTS: f32 = 350.0;
/// ⑦ 滚动条。
const H_SCROLLBAR: f32 = 200.0;
/// ⑧ 表格。
const H_TABLE: f32 = 320.0;
/// ⑨ 树。
const H_TREE: f32 = 290.0;
/// ⑩ 输入框。
const H_INPUTS: f32 = 250.0;
/// ⑪ markdown。
const H_MARKDOWN: f32 = 620.0;
/// ⑫ 浮层。
const H_OVERLAYS: f32 = 150.0;
/// ⑬ 对话框。
const H_DIALOGS: f32 = 120.0;
/// ⑭ 颜色选择器。
const H_PICKER: f32 = 110.0;
/// ⑮ 事件日志。
const H_LOG: f32 = 240.0;

/// markdown 演示文档。
const MARKDOWN_SOURCE: &str = r#"# Markdown 视图

**粗体**、*斜体*、~~删除线~~、`inline code`、[链接](https://example.com)。

## 二级标题

1. 列表项一
2. 列表项二
   - 嵌套项

> 引用块：行内图片走占位绘制（资源通路未接线）。

## 代码

```rust
fn main() {
    println!("gallery");
}
```

## 表格

| 列 A | 列 B | 列 C |
|------|------|------|
| 1 | 2 | 3 |
| 4 | 5 | 6 |

行内图片：![icon](file://gallery-icon.png?size=24)
"#;

// ─────────────────────────────────────────────────────────────────────────────
// 运行参数
// ─────────────────────────────────────────────────────────────────────────────

/// 命令行验收钩子。
#[derive(Clone, Copy, Default)]
struct Flags {
    /// 自动派发一次主按钮动作。
    auto_demo: bool,
    /// 空闲 3 秒帧增量检查（I3）。
    idle_check: bool,
    /// 到点自动关窗（秒）。
    auto_close: Option<u64>,
    /// 打印关键节点布局盒（计算外部点击坐标用）。
    probe: bool,
    /// 打开后自动把页面滚到指定纵向偏移（无人值守截图用；避开滚轮路径的引擎缺陷）。
    scroll_to: Option<f32>,
}

/// 解析 `--flag` / `--flag=value` 形式的参数（未知参数忽略并打印）。
fn parse_flags() -> Flags {
    let mut flags = Flags::default();
    for arg in std::env::args().skip(1) {
        if arg == "--auto-demo" {
            flags.auto_demo = true;
        } else if arg == "--idle-check" {
            flags.idle_check = true;
        } else if let Some(value) = arg.strip_prefix("--auto-close=") {
            flags.auto_close = value.parse::<u64>().ok();
        } else if arg == "--probe" {
            flags.probe = true;
        } else if let Some(value) = arg.strip_prefix("--scroll-to=") {
            flags.scroll_to = value.parse::<f32>().ok();
        } else {
            println!("gallery: ignoring unknown argument {arg}");
        }
    }
    flags
}

/// 空闲检查结果（`--idle-check`）。
#[derive(Debug, Default)]
struct IdleReport {
    /// 1 s 时的帧号。
    baseline: Option<u32>,
    /// 4 s 时的帧号。
    final_frame: Option<u32>,
    /// 3 s 等待期内的帧增量。
    frames: u32,
    /// 是否通过（增量 ≤ 2）。
    pass: Option<bool>,
}

// ─────────────────────────────────────────────────────────────────────────────
// 节点 / 主题小工具
// ─────────────────────────────────────────────────────────────────────────────

/// 场景窗口的节点树。
fn tree(app: &mut App, window: WindowId) -> &mut NodeTree {
    app.window_mut(window)
        .expect("gallery window must be alive")
        .tree_mut()
}

/// 常用文本样式。
fn text_style(size: f32, weight: u16, color: Color) -> TextStyle {
    TextStyle::new(size).with_weight(weight).with_color(color)
}

/// 固定样式容器（流内子节点用）。
fn container(app: &mut App, window: WindowId, parent: NodeId, style: Style) -> NodeId {
    NodeBuilder::boxed()
        .style(style)
        .build(tree(app, window), parent)
        .expect("container must have a live parent")
}

/// 一行：flex_row + 间距 + 垂直居中。
fn row(app: &mut App, window: WindowId, parent: NodeId, gap: f32) -> NodeId {
    container(
        app,
        window,
        parent,
        StyleBuilder::new()
            .flex_row()
            .items_center()
            .gap(gap)
            .width_full()
            .build(),
    )
}

/// 固定宽高的流内盒子。
fn sized_box(app: &mut App, window: WindowId, parent: NodeId, width: f32, height: f32) -> NodeId {
    container(
        app,
        window,
        parent,
        StyleBuilder::new().width(width).height(height).build(),
    )
}

/// 一段**跟随主题**的文字：`foreground` 由 `bind_style` 从当前外观解析。
fn themed_label(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    text: impl Into<SmolStr>,
    size: f32,
    weight: u16,
    role: Role,
    theme: &ThemeHandle,
) -> NodeId {
    let initial = theme.theme_for(Appearance::Dark).role(role);
    let theme = theme.clone();
    NodeBuilder::text(text, text_style(size, weight, initial))
        .bind_style(move |cx| StylePatch {
            foreground: Some(theme.role(cx, role)),
            ..StylePatch::default()
        })
        .build(tree(app, window), parent)
        .expect("themed label must have a live parent")
}

/// 事件日志追加一行（最多 [`LOG_CAPACITY`] 条，超出丢最旧）。
fn log_line(app: &mut App, log: &Signal<Vec<SmolStr>>, line: impl Into<SmolStr>) {
    let line = line.into();
    app.update(log, |lines| {
        lines.push(line);
        if lines.len() > LOG_CAPACITY {
            lines.remove(0);
        }
    });
}

/// 建一个分区：跟随主题的卡片背景 / 边框 / 圆角 + 左侧标题；返回内容体节点。
fn section(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    index: u32,
    title: &'static str,
    height: f32,
    theme: &ThemeHandle,
) -> NodeId {
    let style = StyleBuilder::new()
        .flex_col()
        .flex_none()
        .width(SECTION_W)
        .height(height)
        .padding_sides(14.0, 16.0, 14.0, 16.0)
        .gap(8.0)
        .relative()
        .overflow_hidden()
        .build();
    let card = NodeBuilder::boxed()
        .id(format_smolstr!("gallery.section.{index}"))
        .style(style)
        .paint(PaintBuilder::new().rounded(12.0).build())
        .bind_style({
            let theme = theme.clone();
            move |cx| StylePatch {
                background: Some(theme.role(cx, Role::SurfaceRaised)),
                border: Some(Border {
                    width: 1.0,
                    color: theme.role(cx, Role::LineBorder),
                }),
                ..StylePatch::default()
            }
        })
        .build(tree(app, window), parent)
        .expect("section card must have a live parent");
    themed_label(app, window, card, title, 13.5, 700, Role::FgMuted, theme);
    container(
        app,
        window,
        card,
        StyleBuilder::new()
            .flex_col()
            .flex_1()
            .min_height_0()
            .width_full()
            .gap(8.0)
            .build(),
    )
}

/// 建一个 360×176 的滚动区（20 行）+ 自绘滚动条（挂在同一外层上，不随内容滚动）。
fn scroll_demo(app: &mut App, window: WindowId, parent: NodeId, theme: &ThemeHandle) -> ScrollBar {
    let initial = theme.theme_for(Appearance::Dark);
    let fg = initial.role(Role::FgMuted);
    let outer = container(
        app,
        window,
        parent,
        StyleBuilder::new()
            .width(360.0)
            .height(176.0)
            .relative()
            .build(),
    );
    let viewport = container(
        app,
        window,
        outer,
        StyleBuilder::new()
            .width(360.0)
            .height(176.0)
            .relative()
            .overflow_hidden()
            .build(),
    );
    for index in 0..20usize {
        let shade = if index % 2 == 0 { 0x1F } else { 0x26 };
        let _ = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, index as f32 * 26.0, 360.0, 25.0))
            .paint(PaintSpec {
                background: Color::from_rgba8([shade, shade, shade + 8, 0xFF]),
                ..PaintSpec::default()
            })
            .build(tree(app, window), viewport)
            .expect("scroll row");
        let _ = NodeBuilder::text(
            format_smolstr!("滚动行 #{index} — 滚轮 / 拖条 / 轨道点击"),
            text_style(12.0, 400, fg),
        )
        .layout_override(Rect::new(12.0, index as f32 * 26.0 + 4.0, 320.0, 18.0))
        .build(tree(app, window), viewport)
        .expect("scroll label");
    }
    if let Ok(handle) = app.make_scroll(window, viewport, ScrollConfig::default()) {
        let _ = app.set_scroll_content(&handle, Size::new(360.0, 20.0 * 26.0));
    }
    let bar = ScrollBar::new(
        app,
        theme.clone(),
        "gallery.scrollbar",
        ScrollbarAxis::Vertical,
        ScrollbarOptions::default(),
    );
    let _ = bar.build(tree(app, window), outer);
    bar.attach(app, window, viewport);
    bar
}

// ─────────────────────────────────────────────────────────────────────────────
// Gallery
// ─────────────────────────────────────────────────────────────────────────────

/// 画廊全部组件句柄（每帧 `sync` / 事件 `dispatch` 的直接对象）。
#[derive(Clone)]
struct Gallery {
    /// 窗口。
    window: WindowId,
    /// 页面滚动视口（`--probe` 用：滚到底打印底部节点坐标）。
    viewport: NodeId,
    /// 页面内容总高度（`--probe` 换算底部点击坐标用）。
    content_height: f32,
    /// 主题服务。
    theme: ThemeHandle,
    /// 当前外观（与 `ThemeHandle` 的信号同步）。
    appearance: Signal<Appearance>,
    /// 顶栏状态文本。
    status: Signal<SmolStr>,
    /// 交互日志（最多 8 条）。
    log: Signal<Vec<SmolStr>>,
    /// 主按钮（`--auto-demo` 的派发目标）。
    primary_button: Button,
    /// 顶栏与分区里的全部按钮。
    buttons: Vec<Button>,
    /// 开关 / 复选 / 单选。
    toggles: Vec<Toggle>,
    /// 进度条。
    progress: Progress,
    /// 加载指示器（**静态**：见模块文档 I3 说明）。
    spinner: Spinner,
    /// 标签栏。
    tab_bar: TabBar,
    /// 列表（等高 / 变高 / 双端）。
    lists: Vec<List>,
    /// 独立滚动条。
    scroll_bars: Vec<ScrollBar>,
    /// 表格。
    table: Table,
    /// 树。
    tree: Tree,
    /// 输入框。
    inputs: Vec<InputNode>,
    /// 下拉。
    dropdown: Dropdown,
    /// 选择器。
    select: Select,
    /// 菜单。
    menu: Menu,
    /// 上下文菜单。
    context_menu: ContextMenu,
    /// 提示。
    tooltip: Tooltip,
    /// 独立浮层。
    overlay: Overlay,
    /// 普通对话框。
    dialog: Dialog,
    /// alert 对话框。
    alert_dialog: Dialog,
    /// 颜色选择器。
    color_picker: ColorPicker,
    /// 命令行钩子。
    flags: Flags,
    /// 空闲检查结果。
    idle: Rc<RefCell<IdleReport>>,
    /// 关键节点（`--probe` 打印布局盒，便于外部驱动点击）。
    probe_nodes: Vec<(SmolStr, NodeId)>,
}

impl Gallery {
    /// 建窗 + 建树 + 建组件（不注册帧钩子；见 [`Gallery::install`]）。
    fn build(app: &mut App, flags: Flags) -> Self {
        let theme = ThemeHandle::new(app, Appearance::Dark);
        let layer = Layer::new();
        let router = AnimationRouter::new();
        let initial = theme.theme_for(Appearance::Dark);
        let canvas = initial.role(Role::SurfaceCanvas);
        let fg = initial.role(Role::FgDefault);
        let muted = initial.role(Role::FgMuted);

        let window = app.create_window(WindowOptions {
            title: SmolStr::new_static("Kytrix · base_ui kit gallery (P2-5)"),
            size: Size::new(WINDOW_W, WINDOW_H),
            background: canvas,
            ..WindowOptions::default()
        });

        let appearance = app.signal(Appearance::Dark);
        let log = app.signal(Vec::<SmolStr>::new());
        let status = app.signal(SmolStr::new_static("DPI 读取中…"));
        let mut buttons: Vec<Button> = Vec::with_capacity(32);
        let mut chrome_buttons: Vec<Button> = Vec::with_capacity(1);
        let mut primary_button: Option<Button> = None;

        // ── 根 + 顶栏 ────────────────────────────────────────────────────────
        let root = tree(app, window).root();
        tree(app, window)
            .set_style(root, StyleBuilder::new().flex_col().size_full().build())
            .expect("root style");
        let header = NodeBuilder::boxed()
            .id("gallery.header")
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .padding_sides(0.0, SIDE_MARGIN, 0.0, SIDE_MARGIN)
                    .height(HEADER_H)
                    .width_full()
                    .gap(14.0)
                    .build(),
            )
            .build(tree(app, window), root)
            .expect("header");
        themed_label(
            app,
            window,
            header,
            "Kytrix · base_ui kit gallery",
            18.0,
            700,
            Role::FgDefault,
            &theme,
        );
        themed_label(
            app,
            window,
            header,
            "引擎化 kit 组件 · 15 分区 · 交互日志",
            12.0,
            400,
            Role::FgMuted,
            &theme,
        );
        let _spacer = container(app, window, header, StyleBuilder::new().flex_1().build());

        // 顶栏状态文本（DPI / 客户区 / Esc 提示；内容随窗口变化，绑定信号）。
        let status_signal = status;
        let status_theme = theme.clone();
        NodeBuilder::text("", text_style(11.5, 400, muted))
            .bind_text(move |cx| cx.read(&status_signal).clone())
            .bind_style(move |cx| StylePatch {
                foreground: Some(status_theme.role(cx, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree(app, window), header)
            .expect("status label");

        // 深浅切换按钮（同时更新 `ThemeHandle` 与窗口背景色）。
        let theme_button = Button::new(app, theme.clone(), "gallery.theme-toggle")
            .secondary()
            .label("切换深浅色")
            .on_click({
                let theme = theme.clone();
                move |ctx| {
                    let app = ctx.app_mut();
                    let current = *app.read(&appearance);
                    let next = if current == Appearance::Dark {
                        Appearance::Light
                    } else {
                        Appearance::Dark
                    };
                    app.write(&appearance, next);
                    theme.set_appearance(app, next);
                    if let Some(target) = app.window_mut(window) {
                        target
                            .set_background_color(theme.theme_for(next).role(Role::SurfaceCanvas));
                    }
                    log_line(app, &log, format_smolstr!("theme: 切换为 {next:?}"));
                }
            });
        let theme_button_node = theme_button
            .build(app, window, header)
            .expect("theme button");
        chrome_buttons.push(theme_button);

        // ── 滚动视口 + 内容列 ────────────────────────────────────────────────
        let viewport = NodeBuilder::boxed()
            .id("gallery.viewport")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .width_full()
                    .min_height_0()
                    .relative()
                    .overflow_hidden()
                    .build(),
            )
            .build(tree(app, window), root)
            .expect("viewport");
        // 内容列的高度必须显式给足：引擎的 scroll 容器只知道内容尺寸（`set_scroll_content`），
        // 而 taffy 默认 `flex_shrink: 1` 会把没有显式高度的卡片压扁到视口高。
        let section_heights = [
            H_THEME,
            H_BUTTONS,
            H_TOGGLES,
            H_FEEDBACK,
            H_TABS,
            H_LISTS,
            H_SCROLLBAR,
            H_TABLE,
            H_TREE,
            H_INPUTS,
            H_MARKDOWN,
            H_OVERLAYS,
            H_DIALOGS,
            H_PICKER,
            H_LOG,
        ];
        let total_height = CONTENT_TOP
            + section_heights.iter().sum::<f32>()
            + SECTION_GAP * (section_heights.len() as f32 - 1.0)
            + CONTENT_BOTTOM;
        let content = NodeBuilder::boxed()
            .id("gallery.content")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .flex_none()
                    .width_full()
                    .height(total_height)
                    .padding_sides(CONTENT_TOP, SIDE_MARGIN, CONTENT_BOTTOM, SIDE_MARGIN)
                    .gap(SECTION_GAP)
                    .build(),
            )
            .build(tree(app, window), viewport)
            .expect("content");

        let mut toggles: Vec<Toggle> = Vec::with_capacity(10);
        let mut inputs: Vec<InputNode> = Vec::with_capacity(4);
        let mut lists: Vec<List> = Vec::with_capacity(3);
        let mut scroll_bars: Vec<ScrollBar> = Vec::with_capacity(1);

        // ① 主题 / 图标 / 品牌 ───────────────────────────────────────────────
        let s1 = section(
            app,
            window,
            content,
            1,
            "① 主题色卡 / 图标（55）/ 品牌（7）",
            H_THEME,
            &theme,
        );
        let swatches = row(app, window, s1, 8.0);
        for (name, role) in [
            ("Canvas", Role::SurfaceCanvas),
            ("Raised", Role::SurfaceRaised),
            ("Sunken", Role::SurfaceSunken),
            ("Control", Role::SurfaceControl),
            ("Brand", Role::BrandBase),
            ("Danger", Role::StatusDangerBase),
            ("Success", Role::StatusSuccessBase),
        ] {
            let wrapper = container(
                app,
                window,
                swatches,
                StyleBuilder::new().flex_col().width(92.0).gap(4.0).build(),
            );
            let swatch_theme = theme.clone();
            NodeBuilder::boxed()
                .style(StyleBuilder::new().width_full().height(34.0).build())
                .paint(PaintBuilder::new().rounded(6.0).build())
                .bind_style(move |cx| StylePatch {
                    background: Some(swatch_theme.role(cx, role)),
                    border: Some(Border {
                        width: 1.0,
                        color: swatch_theme.role(cx, Role::LineBorder),
                    }),
                    ..StylePatch::default()
                })
                .build(tree(app, window), wrapper)
                .expect("swatch");
            themed_label(app, window, wrapper, name, 10.5, 500, Role::FgMuted, &theme);
        }
        let icon_color = initial.role(Role::FgMuted);
        for chunk in IconKind::ALL.chunks(19) {
            let icons_row = row(app, window, s1, 8.0);
            for kind in chunk {
                let _ = IconSpec::new(*kind)
                    .with_size(16.0)
                    .with_color(icon_color)
                    .build(app, window, icons_row);
            }
        }
        // 品牌 logo 资产按深色背景设计（浅色主题下部分白/浅色 logo 不可见）：
        // 示例固定给一块深色底板，让 7 个 logo 在两种外观下都可辨认（资产批次应补浅色变体）。
        let brand_plate = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(16.0)
                    .padding_sides(6.0, 14.0, 6.0, 14.0)
                    .height(46.0)
                    .build(),
            )
            .paint(PaintSpec {
                background: Color::from_rgba8([0x14, 0x1A, 0x22, 0xFF]),
                corner_radius: [6.0; 4],
                ..PaintSpec::default()
            })
            .build(tree(app, window), s1)
            .expect("brand plate");
        for kind in BrandKind::ALL {
            let _ = Brand::new(kind)
                .with_size(26.0)
                .with_alt(kind.name())
                .build(app, window, brand_plate);
        }

        // ② 按钮 ─────────────────────────────────────────────────────────────
        let s2 = section(
            app,
            window,
            content,
            2,
            "② 按钮（5 变体 × 3 尺寸 + 禁用 / 加载 / 图标 / 选中）",
            H_BUTTONS,
            &theme,
        );
        let mut button_index = 0usize;
        for size in [UnifiedSize::Small, UnifiedSize::Medium, UnifiedSize::Large] {
            let buttons_row = row(app, window, s2, 8.0);
            for variant in [
                ButtonVariant::Primary,
                ButtonVariant::Secondary,
                ButtonVariant::Outline,
                ButtonVariant::Ghost,
                ButtonVariant::Danger,
            ] {
                button_index += 1;
                let button =
                    make_button(app, &theme, log, button_index, size, variant, false, false);
                let _ = button.build(app, window, buttons_row);
                if primary_button.is_none() && variant == ButtonVariant::Primary {
                    primary_button = Some(button.clone());
                }
                buttons.push(button);
            }
            button_index += 1;
            let disabled = make_button(
                app,
                &theme,
                log,
                button_index,
                size,
                ButtonVariant::Secondary,
                true,
                false,
            );
            let _ = disabled.build(app, window, buttons_row);
            buttons.push(disabled);
            button_index += 1;
            let loading = make_button(
                app,
                &theme,
                log,
                button_index,
                size,
                ButtonVariant::Primary,
                false,
                true,
            );
            let _ = loading.build(app, window, buttons_row);
            buttons.push(loading);
            button_index += 1;
            let with_icon = Button::new(
                app,
                theme.clone(),
                format_smolstr!("gallery.btn.{button_index}"),
            )
            .primary()
            .with_unified_size(size)
            .icon(IconKind::Check)
            .label("图标")
            .on_click(move |ctx| log_line(ctx.app_mut(), &log, "button: 图标按钮点击"));
            let _ = with_icon.build(app, window, buttons_row);
            buttons.push(with_icon);
            button_index += 1;
            let selected = Button::new(
                app,
                theme.clone(),
                format_smolstr!("gallery.btn.{button_index}"),
            )
            .secondary()
            .with_unified_size(size)
            .label("选中")
            .selected(true)
            .on_click(move |ctx| log_line(ctx.app_mut(), &log, "button: 选中按钮点击"));
            let _ = selected.build(app, window, buttons_row);
            buttons.push(selected);
        }

        // ③ 开关 / 复选 / 单选 ───────────────────────────────────────────────
        let s3 = section(
            app,
            window,
            content,
            3,
            "③ 开关 / 复选 / 单选（Toggle：Space / Enter 激活）",
            H_TOGGLES,
            &theme,
        );
        for (kind, labels) in [
            ("switch", &["通知", "自动刷新", "紧凑模式"][..]),
            ("checkbox", &["显示网格", "显示坐标轴"][..]),
            ("radio", &["浅色", "深色"][..]),
        ] {
            let toggle_row = row(app, window, s3, 20.0);
            for (index, text) in labels.iter().enumerate() {
                let mut toggle = Toggle::new(
                    app,
                    theme.clone(),
                    format_smolstr!("gallery.toggle.{kind}.{index}"),
                )
                .label(*text)
                .checked(index == 0)
                .on_change({
                    let text = *text;
                    move |checked, app| {
                        log_line(app, &log, format_smolstr!("toggle[{text}]: {checked}"));
                    }
                });
                toggle = match kind {
                    "checkbox" => toggle.checkbox(),
                    "radio" => toggle.radio(),
                    _ => toggle.switch(),
                };
                let _ = toggle.build(app, window, toggle_row);
                toggles.push(toggle);
            }
        }

        // ④ 反馈控件 ─────────────────────────────────────────────────────────
        let s4 = section(
            app,
            window,
            content,
            4,
            "④ 标签 / 进度 / 加载 / 分割线 / 空态",
            H_FEEDBACK,
            &theme,
        );
        let badges_row = row(app, window, s4, 10.0);
        for (text, badge) in [
            ("Neutral", Badge::new(theme.clone()).neutral()),
            ("Info", Badge::new(theme.clone()).info()),
            ("Success", Badge::new(theme.clone()).success()),
            ("Warning", Badge::new(theme.clone()).warning()),
            ("Elevated", Badge::new(theme.clone()).elevated()),
            ("Error", Badge::new(theme.clone()).error()),
        ] {
            let _ = badge.text(text).build(tree(app, window), badges_row);
        }
        let feedback_row = row(app, window, s4, 16.0);
        let progress_box = sized_box(app, window, feedback_row, 280.0, 36.0);
        let progress = Progress::new(app, theme.clone(), "gallery.progress")
            .value(42.0)
            .label("进度 42%");
        let _ = progress.build(app, window, progress_box);
        let spinner = Spinner::new(app, theme.clone(), "gallery.spinner")
            .with_size(18.0)
            .with_color(initial.role(Role::BrandBase))
            .with_label("静态加载指示（动画默认关闭，保 I3）")
            .spinning(false);
        let _ = spinner.build(app, window, feedback_row);
        let separator_box = sized_box(app, window, feedback_row, 160.0, 20.0);
        let _ = Separator::horizontal(theme.clone()).build(tree(app, window), separator_box);
        let empty_box = sized_box(app, window, feedback_row, 340.0, 36.0);
        let _ = EmptyState::new(theme.clone(), "gallery.empty")
            .text("空态：这里没有内容")
            .build(tree(app, window), empty_box);

        // ⑤ 标签栏 ───────────────────────────────────────────────────────────
        let s5 = section(
            app,
            window,
            content,
            5,
            "⑤ 标签栏（TabBar：方向键 / 指示条 / 关闭按钮）",
            H_TABS,
            &theme,
        );
        let tab_bar = TabBar::new(
            app,
            theme.clone(),
            "gallery.tabs",
            [
                TabItem::new("概览"),
                TabItem::new("组件"),
                TabItem::new("日志"),
                TabItem::new("设置"),
            ],
            TabBarOptions {
                size: UnifiedSize::Medium,
                indicator: IndicatorAnimation::Slide,
                closable: true,
            },
        )
        .on_change({
            move |index, app| log_line(app, &log, format_smolstr!("tab: 选中 #{index}"))
        })
        .on_close({
            move |index, app| {
                log_line(
                    app,
                    &log,
                    format_smolstr!("tab: 关闭 #{index}（示例仅记录）"),
                )
            }
        });
        let _ = tab_bar.build(app, window, s5);

        // ⑥ 列表 ─────────────────────────────────────────────────────────────
        let s6 = section(
            app,
            window,
            content,
            6,
            "⑥ 列表（等高 / 变高 / 双端 deque + 自绘滚动条）",
            H_LISTS,
            &theme,
        );
        let list_controls = row(app, window, s6, 10.0);
        let deque_data: Rc<RefCell<Vec<SmolStr>>> = Rc::new(RefCell::new(Vec::with_capacity(32)));
        for index in 0..5usize {
            deque_data
                .borrow_mut()
                .push(format_smolstr!("deque 事件 #{index}"));
        }
        let deque_list_slot: Rc<RefCell<Option<List>>> = Rc::new(RefCell::new(None));
        let deque_button = Button::new(app, theme.clone(), "gallery.deque.push")
            .secondary()
            .with_unified_size(UnifiedSize::Small)
            .label("双端列表 push")
            .on_click({
                let data = deque_data.clone();
                let slot = deque_list_slot.clone();
                move |ctx| {
                    let app = ctx.app_mut();
                    {
                        let mut data = data.borrow_mut();
                        if data.len() >= 32 {
                            data.remove(0);
                        }
                        let item = format_smolstr!("deque 事件 #{}", data.len());
                        data.push(item);
                    }
                    if let Some(list) = slot.borrow().clone() {
                        let _ = list.push(app);
                    }
                    log_line(app, &log, "deque: push 一条（容量 32，满则丢最旧）");
                }
            });
        let _ = deque_button.build(app, window, list_controls);
        buttons.push(deque_button);

        let list_columns = row(app, window, s6, 12.0);
        let fixed_style = text_style(11.5, 400, muted);
        let fixed_list = List::new(
            app,
            theme.clone(),
            "gallery.list.fixed",
            ListOptions::fixed(26.0),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                NodeBuilder::text(format_smolstr!("等高行 #{index}"), fixed_style.clone())
                    .style(StyleBuilder::new().width_full().build())
                    .build(tree, parent)?;
                Ok(())
            },
        );
        let variable_style = text_style(11.5, 400, Color::from_rgba8([0xC8, 0xD0, 0xDA, 0xFF]))
            .with_wrap(TextWrap::Wrap);
        let variable_list = List::new(
            app,
            theme.clone(),
            "gallery.list.variable",
            ListOptions::variable(26.0),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                let lines = 1 + index % 3;
                let mut text = format_smolstr!("变高行 #{index}：");
                for _ in 0..lines {
                    text = format_smolstr!("{text}测量回写内容 ");
                }
                NodeBuilder::text(text, variable_style.clone())
                    .style(StyleBuilder::new().width_full().build())
                    .build(tree, parent)?;
                Ok(())
            },
        );
        let deque_style = text_style(11.5, 400, muted);
        let deque_list = {
            let data = deque_data.clone();
            List::new(
                app,
                theme.clone(),
                "gallery.list.deque",
                ListOptions::fixed(26.0).deque(32),
                move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                    let text = data
                        .borrow()
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| format_smolstr!("deque #{index}"));
                    NodeBuilder::text(text, deque_style.clone())
                        .style(StyleBuilder::new().width_full().build())
                        .build(tree, parent)?;
                    Ok(())
                },
            )
        };
        for list in [&fixed_list, &variable_list, &deque_list] {
            let holder = sized_box(app, window, list_columns, 360.0, 240.0);
            let _ = list.mount(app, window, holder);
        }
        let _ = fixed_list.set_items(app, 8);
        let _ = variable_list.set_items(app, 6);
        let _ = deque_list.set_items(app, deque_data.borrow().len());
        *deque_list_slot.borrow_mut() = Some(deque_list.clone());
        lists.extend([fixed_list, variable_list, deque_list]);

        // ⑦ 独立滚动条 ───────────────────────────────────────────────────────
        let s7 = section(
            app,
            window,
            content,
            7,
            "⑦ 自绘滚动条（ScrollBar + 引擎 scroll 容器）",
            H_SCROLLBAR,
            &theme,
        );
        let scroll_row = row(app, window, s7, 16.0);
        scroll_bars.push(scroll_demo(app, window, scroll_row, &theme));
        themed_label(
            app,
            window,
            scroll_row,
            "拖拽滑块 / 点击轨道 / 滚轮",
            12.0,
            400,
            Role::FgMuted,
            &theme,
        );

        // ⑧ 表格 ─────────────────────────────────────────────────────────────
        let s8 = section(
            app,
            window,
            content,
            8,
            "⑧ 表格（跨列 / 可排序表头 / 多选 / 变高行）",
            H_TABLE,
            &theme,
        );
        let table_holder = sized_box(app, window, s8, 1100.0, 250.0);
        let columns = vec![
            Column::fixed("名称", 150.0).sortable(),
            Column::fixed("类型", 110.0).sortable(),
            Column::flex("描述", 2.0),
            Column::fixed("标签", 130.0).span(2),
            Column::fixed("数值", 90.0).align(ColumnAlign::End),
        ];
        let table = Table::new(
            app,
            theme.clone(),
            "gallery.table",
            columns,
            TableOptions::variable(30.0).selection(SelectionMode::Multi),
            |tree: &mut NodeTree, parent: NodeId, row: usize, column: usize| {
                let text = match column {
                    0 => format_smolstr!("资产 #{row}"),
                    1 => {
                        if row.is_multiple_of(2) {
                            SmolStr::new_static("现货")
                        } else {
                            SmolStr::new_static("合约")
                        }
                    }
                    2 => {
                        if row.is_multiple_of(2) {
                            format_smolstr!("第 {row} 行的长描述：变高表格按内容实测回写行高（E3）")
                        } else {
                            format_smolstr!("短描述 {row}")
                        }
                    }
                    3 => format_smolstr!("跨列标签 {row}"),
                    _ => format_smolstr!("{}", row * 100),
                };
                NodeBuilder::text(
                    text,
                    text_style(11.5, 400, Color::from_rgba8([0xD8, 0xDE, 0xE6, 0xFF])),
                )
                .style(StyleBuilder::new().width_full().build())
                .build(tree, parent)
                .expect("table cell");
            },
            "表格无数据",
        );
        let _ = table.mount(app, window, table_holder);
        let _ = table.set_rows(app, 8);
        table.on_sort({
            move |app, sort| {
                let text = match sort {
                    Some((column, order)) => format_smolstr!("table: 排序 列 {column} {order:?}"),
                    None => SmolStr::new_static("table: 取消排序"),
                };
                log_line(app, &log, text);
            }
        });
        table.on_row_activate({
            move |app, row| log_line(app, &log, format_smolstr!("table: 激活行 {row}"))
        });

        // ⑨ 树 ───────────────────────────────────────────────────────────────
        let s9 = section(
            app,
            window,
            content,
            9,
            "⑨ 树（展开折叠 / 层级线 / 方向键 / 单选）",
            H_TREE,
            &theme,
        );
        let tree_holder = sized_box(app, window, s9, 480.0, 220.0);
        let roots = [
            TreeItem::new("src", "src")
                .child(TreeItem::new("src/main", "main.rs"))
                .child(
                    TreeItem::new("src/kit", "kit/")
                        .child(TreeItem::new("src/kit/list", "list.rs")),
                )
                .child(TreeItem::new("src/theme", "theme/")),
            TreeItem::new("docs", "docs")
                .child(TreeItem::new("docs/spec", "spec-deltas/"))
                .child(TreeItem::new("docs/readme", "README.md")),
            TreeItem::new("Cargo", "Cargo.toml"),
        ];
        let tree_view = Tree::new(
            app,
            theme.clone(),
            "gallery.tree",
            roots,
            TreeOptions::default(),
        );
        let _ = tree_view.mount(app, window, tree_holder);
        if !tree_view.state(app).is_expanded("src") {
            let _ = tree_view.toggle(app, "src");
        }
        if !tree_view.state(app).is_expanded("src/kit") {
            let _ = tree_view.toggle(app, "src/kit");
        }
        if !tree_view.state(app).is_expanded("docs") {
            let _ = tree_view.toggle(app, "docs");
        }

        // ⑩ 输入框 ───────────────────────────────────────────────────────────
        let s10 = section(
            app,
            window,
            content,
            10,
            "⑩ 输入框（单行 / 掩码 / 禁用 / 多行 + IME）",
            H_INPUTS,
            &theme,
        );
        let input_row = row(app, window, s10, 12.0);
        let single_box = sized_box(app, window, input_row, 300.0, 40.0);
        let single = make_input(
            app,
            window,
            single_box,
            &theme,
            "gallery.input.single",
            "单行输入…",
            false,
            false,
            false,
            ControlSize::Medium,
        );
        let masked_box = sized_box(app, window, input_row, 240.0, 40.0);
        let masked = make_input(
            app,
            window,
            masked_box,
            &theme,
            "gallery.input.masked",
            "密码 / 掩码",
            true,
            false,
            false,
            ControlSize::Medium,
        );
        let disabled_box = sized_box(app, window, input_row, 240.0, 40.0);
        let disabled = make_input(
            app,
            window,
            disabled_box,
            &theme,
            "gallery.input.disabled",
            "禁用输入框",
            false,
            true,
            false,
            ControlSize::Medium,
        );
        inputs.extend([single, masked, disabled]);
        let multiline_row = row(app, window, s10, 12.0);
        let multiline_box = sized_box(app, window, multiline_row, 620.0, 96.0);
        let multiline = make_input(
            app,
            window,
            multiline_box,
            &theme,
            "gallery.input.multiline",
            "多行输入：Enter 换行，Ctrl+Z 撤销…",
            false,
            false,
            true,
            ControlSize::Small,
        );
        inputs.push(multiline);
        themed_label(
            app,
            window,
            s10,
            "IME：点击任一输入框后窗口级 IME 打开（焦点在输入框时 set_ime_cursor_area）。",
            11.5,
            400,
            Role::FgMuted,
            &theme,
        );

        // ⑪ markdown ─────────────────────────────────────────────────────────
        let s11 = section(
            app,
            window,
            content,
            11,
            "⑪ markdown（富文本 / 代码 / 表格 / 行内图片占位）",
            H_MARKDOWN,
            &theme,
        );
        let doc = Arc::new(parse_document(MARKDOWN_SOURCE, &NoAssets));
        let view = MarkdownView::from_theme(theme.theme_for(Appearance::Dark), doc);
        let _markdown = view.build(tree(app, window), s11);

        // ⑫ 浮层 ─────────────────────────────────────────────────────────────
        let s12 = section(
            app,
            window,
            content,
            12,
            "⑫ 浮层（Overlay / Tooltip / Dropdown / Select / Menu / ContextMenu）",
            H_OVERLAYS,
            &theme,
        );
        let overlay_row = row(app, window, s12, 10.0);

        // 独立 Overlay：触发节点自己声明动作（`ActionCtx::node` 即锚点）。
        let overlay = Overlay::new(
            app,
            theme.clone(),
            layer.clone(),
            router.clone(),
            "gallery.overlay",
            OverlayOptions {
                band: Band::Popup,
                ..OverlayOptions::default()
            },
        );
        let overlay_action = Action::Custom(SmolStr::new_static("gallery.overlay.open"));
        let _overlay_trigger = demo_trigger(
            app,
            window,
            overlay_row,
            &theme,
            "打开 Overlay",
            overlay_action.clone(),
        );
        app.on_action(overlay_action, {
            let overlay = overlay.clone();
            move |ctx| {
                let node = ctx.node();
                let window = ctx.window();
                let app = ctx.app_mut();
                let anchor = Anchor::node(node);
                let opened = overlay.open(app, window, anchor, |tree, panel| {
                    NodeBuilder::text(
                        "Overlay 面板（12 向 placement + flip/clamp + 淡入淡出）",
                        text_style(12.0, 400, Color::from_rgba8([0xE6, 0xE8, 0xEB, 0xFF])),
                    )
                    .style(StyleBuilder::new().padding(8.0).build())
                    .build(tree, panel)?;
                    Ok(())
                });
                if opened.is_ok() {
                    log_line(app, &log, "overlay: 打开（锚点=触发节点）");
                }
            }
        });

        // Tooltip：挂在一个按钮上。
        let tooltip = Tooltip::new(
            app,
            theme.clone(),
            layer.clone(),
            router.clone(),
            "gallery.tooltip",
            TooltipOptions::default(),
        );
        tooltip.set_content(Some(
            TooltipContent::new("提示：hover 500 ms 后出现").shortcut("Ctrl+T"),
        ));
        let tooltip_button = Button::new(app, theme.clone(), "gallery.tooltip.trigger")
            .outline()
            .with_unified_size(UnifiedSize::Small)
            .label("Tooltip 触发器")
            .on_click(move |ctx| log_line(ctx.app_mut(), &log, "tooltip: 触发器点击"));
        let tooltip_node = tooltip_button
            .build(app, window, overlay_row)
            .expect("tooltip trigger");
        tooltip.attach(app, window, tooltip_node);
        buttons.push(tooltip_button);

        // Dropdown。
        let dropdown = Dropdown::new(
            app,
            theme.clone(),
            layer.clone(),
            router.clone(),
            "gallery.dropdown",
            DropdownOptions::default(),
        )
        .on_select({
            move |app, id| log_line(app, &log, format_smolstr!("dropdown: 选中 {id}"))
        });
        let _ = dropdown.set_items(
            app,
            [
                DropdownItem::new("a", "选项 A").icon(IconKind::Check),
                DropdownItem::new("b", "选项 B"),
                DropdownItem::new("c", "选项 C（禁用）").disabled(true),
            ],
        );
        let _ = dropdown.mount(app, window, overlay_row);

        // Select（多选 + 标签）。
        let select = Select::new(
            app,
            theme.clone(),
            layer.clone(),
            router.clone(),
            "gallery.select",
            SelectOptions::default().multiple(true).clearable(true),
        )
        .on_change({
            move |app, chosen| log_line(app, &log, format_smolstr!("select: 选中 {chosen:?}"))
        });
        let _ = select.set_items(
            app,
            [
                SelectItem::new("d1", "标签一").icon(IconKind::Circle),
                SelectItem::new("d2", "标签二"),
                SelectItem::new("d3", "标签三"),
                SelectItem::new("d4", "标签四"),
                SelectItem::new("d5", "标签五"),
            ],
        );
        let _ = select.mount(app, window, overlay_row);

        // Menu（触发节点自声明动作；打开锚定到触发节点）。
        let menu = Menu::new(
            app,
            theme.clone(),
            layer.clone(),
            router.clone(),
            "gallery.menu",
            [
                MenuEntry::Group(SmolStr::new_static("编辑")),
                MenuEntry::Action(MenuAction::new("copy", "复制").shortcut("Ctrl+C")),
                MenuEntry::Action(MenuAction::new("paste", "粘贴").shortcut("Ctrl+V")),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::new("delete", "删除").danger(true)),
                MenuEntry::Submenu(MenuSubmenu::new(
                    "更多",
                    [
                        MenuEntry::Action(MenuAction::new("export", "导出")),
                        MenuEntry::Action(MenuAction::new("settings", "设置").disabled(true)),
                    ],
                )),
            ],
            MenuOptions::default(),
        )
        .on_activate({
            move |app, activation| {
                log_line(app, &log, format_smolstr!("menu: 激活 {}", activation.id))
            }
        });
        let menu_action = Action::Custom(SmolStr::new_static("gallery.menu.open"));
        let _menu_trigger = demo_trigger(
            app,
            window,
            overlay_row,
            &theme,
            "打开 Menu",
            menu_action.clone(),
        );
        app.on_action(menu_action, {
            let menu = menu.clone();
            move |ctx| {
                let node = ctx.node();
                let window = ctx.window();
                let app = ctx.app_mut();
                let opened = menu.open(app, window, Anchor::node(node));
                if opened.is_ok() {
                    log_line(app, &log, "menu: 打开（↑↓ / Enter / Esc / 子菜单 →）");
                }
            }
        });

        // ContextMenu（触发节点自声明动作；锚点取触发节点几何）。
        let context_menu = ContextMenu::new(
            app,
            theme.clone(),
            layer.clone(),
            router.clone(),
            "gallery.context-menu",
            [
                MenuEntry::Action(MenuAction::new("inspect", "查看")),
                MenuEntry::Action(MenuAction::new("duplicate", "复制一份")),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::new("remove", "移除").danger(true)),
            ],
            ContextMenuOptions::default(),
        )
        .on_item_click({
            move |app, id| log_line(app, &log, format_smolstr!("context-menu: 点击 {id}"))
        });
        let context_action = Action::Custom(SmolStr::new_static("gallery.context.open"));
        let _context_trigger = demo_trigger(
            app,
            window,
            overlay_row,
            &theme,
            "打开 ContextMenu",
            context_action.clone(),
        );
        app.on_action(context_action, {
            let context_menu = context_menu.clone();
            move |ctx| {
                let node = ctx.node();
                let window = ctx.window();
                let app = ctx.app_mut();
                let point = app
                    .window(window)
                    .and_then(|target| target.tree().get(node).ok())
                    .map(|node| {
                        let bounds = node.layout().bounds;
                        Point::new(
                            bounds.origin.x + 24.0,
                            bounds.origin.y + bounds.size.height + 6.0,
                        )
                    })
                    .unwrap_or(Point::new(420.0, 600.0));
                let opened = context_menu.open(app, window, point);
                if opened.is_ok() {
                    log_line(app, &log, "context-menu: 打开（按钮演示右键语义）");
                }
            }
        });

        // ⑬ 对话框 ───────────────────────────────────────────────────────────
        let s13 = section(
            app,
            window,
            content,
            13,
            "⑬ 对话框（Dialog 模态 + Alert 内容面板）",
            H_DIALOGS,
            &theme,
        );
        let dialog_row = row(app, window, s13, 12.0);
        let dialog = DialogBuilder::new("示例对话框")
            .content(|app, window, body| {
                NodeBuilder::text(
                    "模态对话框：Esc 关闭 / 点遮罩关闭 / 焦点陷阱与恢复。",
                    text_style(12.5, 400, Color::from_rgba8([0xD8, 0xDE, 0xE6, 0xFF]))
                        .with_wrap(TextWrap::Wrap),
                )
                .style(StyleBuilder::new().width_full().build())
                .build(tree(app, window), body)?;
                Ok(())
            })
            .confirm("确定", {
                move |app| log_line(app, &log, "dialog: 确认")
            })
            .cancel("取消", {
                move |app| log_line(app, &log, "dialog: 取消")
            })
            .on_close({
                move |close, app| log_line(app, &log, format_smolstr!("dialog: 关闭 {close:?}"))
            })
            .build(
                app,
                theme.clone(),
                layer.clone(),
                router.clone(),
                "gallery.dialog",
            );
        let dialog_button = Button::new(app, theme.clone(), "gallery.dialog.open")
            .primary()
            .with_unified_size(UnifiedSize::Medium)
            .label("打开对话框")
            .on_click({
                let dialog = dialog.clone();
                move |ctx| {
                    let window = ctx.window();
                    let app = ctx.app_mut();
                    let _ = dialog.open(app, window);
                    log_line(app, &log, "dialog: open()");
                }
            });
        let _ = dialog_button.build(app, window, dialog_row);
        buttons.push(dialog_button);

        let alert_dialog = DialogBuilder::alert("删除确认", move |_dialog, _theme| {
            AlertBuilder::new()
                .ok_cancel("删除文件", "此操作无法撤销，是否继续？")
                .ok_text("删除")
                .cancel_text("保留")
                .ok_action(move |_value, app| log_line(app, &log, "alert: 确认删除"))
                .cancel_action(move |app| log_line(app, &log, "alert: 取消"))
        })
        .build(
            app,
            theme.clone(),
            layer.clone(),
            router.clone(),
            "gallery.alert",
        );
        let alert_button = Button::new(app, theme.clone(), "gallery.alert.open")
            .danger()
            .with_unified_size(UnifiedSize::Medium)
            .label("打开 Alert")
            .on_click({
                let alert_dialog = alert_dialog.clone();
                move |ctx| {
                    let window = ctx.window();
                    let app = ctx.app_mut();
                    let _ = alert_dialog.open(app, window);
                    log_line(app, &log, "alert: open()");
                }
            });
        let _ = alert_button.build(app, window, dialog_row);
        buttons.push(alert_button);

        // ⑭ 颜色选择器 ───────────────────────────────────────────────────────
        let s14 = section(
            app,
            window,
            content,
            14,
            "⑭ 颜色选择器（ColorPicker：色板 / HSLA / HEX）",
            H_PICKER,
            &theme,
        );
        let picker_row = row(app, window, s14, 12.0);
        let color_picker = ColorPicker::new(
            app,
            theme.clone(),
            layer,
            router,
            "gallery.color-picker",
            Color::from_rgba8([0x4C, 0x9A, 0xFF, 0xFF]),
            UnifiedSize::Medium,
        );
        color_picker.on_change(move |app, color| {
            log_line(app, &log, format_smolstr!("color-picker: {color:?}"))
        });

        let _ = color_picker.build(app, window, picker_row);

        // ⑮ 事件日志 ─────────────────────────────────────────────────────────
        let s15 = section(
            app,
            window,
            content,
            15,
            "⑮ 交互日志（最近 8 条：点击 / 切换 / 选择 / 浮层 / 对话框）",
            H_LOG,
            &theme,
        );
        let log_text = log;
        NodeBuilder::text(
            "",
            text_style(12.0, 400, fg)
                .with_wrap(TextWrap::Wrap)
                .with_line_height(1.6),
        )
        .bind_text(move |cx| {
            let lines = cx.read(&log_text);
            let mut text = SmolStr::new_static("");
            for line in lines.iter() {
                text = format_smolstr!("{text}{line}\n");
            }
            if text.is_empty() {
                text = SmolStr::new_static("（尚无事件：点击任意控件）");
            }
            text
        })
        .style(StyleBuilder::new().width_full().flex_1().build())
        .build(tree(app, window), s15)
        .expect("log text");

        match app.make_scroll(window, viewport, ScrollConfig::default()) {
            Ok(handle) => {
                let _ = app.set_scroll_content(&handle, Size::new(WINDOW_W, total_height));
            }
            Err(error) => println!("gallery: WARN viewport scroll registration failed: {error}"),
        }

        // 顶栏按钮并入统一的 dispatch / sync 表。

        buttons.extend(chrome_buttons);
        let mut probe_nodes: Vec<(SmolStr, NodeId)> = Vec::with_capacity(8);
        if let Some(node) = primary_button.as_ref().and_then(Button::node) {
            probe_nodes.push((SmolStr::new_static("primary"), node));
        }
        probe_nodes.push((SmolStr::new_static("theme"), theme_button_node));
        if let Some(node) = toggles.first().and_then(Toggle::node) {
            probe_nodes.push((SmolStr::new_static("toggle0"), node));
        }
        if let Some(node) = dropdown.trigger_node() {
            probe_nodes.push((SmolStr::new_static("dropdown"), node));
        }
        if let Some(node) = select.trigger_node() {
            probe_nodes.push((SmolStr::new_static("select"), node));
        }
        if let Some(node) = color_picker.trigger_node() {
            probe_nodes.push((SmolStr::new_static("color_picker"), node));
        }
        if let Some(node) = dialog.confirm_button().and_then(|button| button.node()) {
            probe_nodes.push((SmolStr::new_static("dialog_confirm"), node));
        }
        for input in &inputs {
            probe_nodes.push((SmolStr::new_static("input"), input.root));
        }
        // 初始日志：证明建树完成。
        log_line(app, &log, "gallery: 建树完成（15 个分区）");

        Self {
            window,
            viewport,
            content_height: total_height,
            theme,
            appearance,
            status,
            log,
            primary_button: primary_button.expect("at least one primary button"),
            buttons,
            toggles,
            progress,
            spinner,
            tab_bar,
            lists,
            scroll_bars,
            table,
            tree: tree_view,
            inputs,
            dropdown,
            select,
            menu,
            context_menu,
            tooltip,
            overlay,
            dialog,
            alert_dialog,
            color_picker,
            flags,
            idle: Rc::new(RefCell::new(IdleReport::default())),
            probe_nodes,
        }
    }

    /// 安装交互：帧钩子（事件 → 组件 dispatch / 每轮 sync）+ 验收定时器。
    fn install(&self, app: &mut App) {
        let mut hook = self.clone();
        app.on_frame(move |app| hook.tick(app));

        let window = self.window;
        if self.flags.probe {
            let nodes = self.probe_nodes.clone();
            let viewport = self.viewport;
            let content_height = self.content_height;
            let lists = self.lists.clone();
            let table = self.table.clone();
            let tree = self.tree.clone();
            let _ = app.after(window, Duration::from_millis(1_500), move |app| {
                let scale = app
                    .window(window)
                    .map(|target| target.scale().factor())
                    .unwrap_or(1.0);
                println!("gallery: probe scale={scale} content_height={content_height}");
                print_probe_rects(app, window, &nodes, "");
                print_probe_mounted(app, "top", &lists, &table, &tree);
                if let Some(handle) = app.scroll_handle(window, viewport) {
                    let _ = app.scroll_to(&handle, Point::new(0.0, 1.0e6));
                }
                let nodes = nodes.clone();
                let lists = lists.clone();
                let table = table.clone();
                let tree = tree.clone();
                let _ = app.after(window, Duration::from_millis(600), move |app| {
                    if let Some(handle) = app.scroll_handle(window, viewport) {
                        let offset = app
                            .scroll_state(&handle)
                            .map(|state| state.offset.y)
                            .unwrap_or(0.0);
                        println!("gallery: probe bottom scroll_offset={offset}");
                    }
                    print_probe_rects(app, window, &nodes, "bottom ");
                    print_probe_mounted(app, "bottom", &lists, &table, &tree);
                    if let Some(handle) = app.scroll_handle(window, viewport) {
                        let _ = app.scroll_to(&handle, Point::new(0.0, 2_300.0));
                    }
                    if let Some((_, node)) = nodes.iter().find(|(name, _)| name == "input") {
                        let _ = app.set_focus(window, Some(*node));
                        println!("gallery: probe focused first input node={node:?}");
                    }
                });
            });
        }
        if let Some(offset) = self.flags.scroll_to {
            let viewport = self.viewport;
            let _ = app.after(window, Duration::from_millis(8_000), move |app| {
                if let Some(handle) = app.scroll_handle(window, viewport) {
                    let _ = app.scroll_to(&handle, Point::new(0.0, offset));
                    println!("gallery: scroll-to offset={offset}");
                }
                app.handle().wake();
            });
        }
        if self.flags.auto_demo {
            let button = self.primary_button.clone();
            let log = self.log;
            let _ = app.after(window, Duration::from_millis(1_200), move |app| {
                let before = app
                    .window(window)
                    .map(|target| target.frame_index().get())
                    .unwrap_or(0);
                let fired = match (button.node(), button.action_identity()) {
                    (Some(node), Some(action)) => {
                        app.dispatch_action(window, node, action, ActionSource::Programmatic)
                    }
                    _ => false,
                };
                log_line(app, &log, "auto-demo: 主按钮动作派发");
                let after = app
                    .window(window)
                    .map(|target| target.frame_index().get())
                    .unwrap_or(0);
                println!(
                    "gallery: auto-demo dispatch fired={fired} frame_index {before} -> {after}"
                );
                let _ = app.after(window, Duration::from_millis(600), move |app| {
                    let frame = app
                        .window(window)
                        .map(|target| target.frame_index().get())
                        .unwrap_or(0);
                    println!(
                        "gallery: auto-demo probe frame_index={frame} (600 ms after dispatch)"
                    );
                });
                app.handle().wake();
            });
        }
        if self.flags.idle_check {
            let idle = self.idle.clone();
            let _ = app.after(window, Duration::from_millis(1_000), move |app| {
                let frame = app
                    .window(window)
                    .map(|target| target.frame_index().get())
                    .unwrap_or(0);
                idle.borrow_mut().baseline = Some(frame);
                println!("gallery: idle-check baseline frame_index={frame}");
                let idle = idle.clone();
                let _ = app.after(window, Duration::from_millis(3_000), move |app| {
                    let frame = app
                        .window(window)
                        .map(|target| target.frame_index().get())
                        .unwrap_or(0);
                    let mut report = idle.borrow_mut();
                    let baseline = report.baseline.unwrap_or(frame);
                    let frames = frame.saturating_sub(baseline);
                    report.final_frame = Some(frame);
                    report.frames = frames;
                    report.pass = Some(frames <= 2);
                    println!(
                        "gallery: idle-check 3000ms frame_index {baseline} -> {frame} (idle frames {frames}, pass={})",
                        frames <= 2
                    );
                    drop(report);
                    app.close_window(window);
                    app.handle().wake();
                });
            });
        }
        if let Some(seconds) = self.flags.auto_close
            && !self.flags.idle_check
        {
            let _ = app.after(window, Duration::from_secs(seconds), move |app| {
                println!("gallery: auto-close after {seconds}s");
                app.close_window(window);
                app.handle().wake();
            });
        }
    }

    /// 每帧：事件分发 → 状态镜像 → 各组件 sync。
    fn tick(&mut self, app: &mut App) {
        if app.window(self.window).is_none() {
            return;
        }
        let events: Vec<Event> = app
            .drain_events()
            .into_iter()
            .filter_map(|(window, event)| (window == self.window).then_some(event))
            .collect();
        for event in &events {
            self.handle_event(app, event);
        }

        // 顶栏状态：DPI / 客户区尺寸（只在变化时真正写信号，避免空闲自触发帧）。
        if let Some(target) = app.window(self.window) {
            let scale = target.scale().factor();
            let size = target.size();
            let text = format_smolstr!(
                "DPI {scale:.2}× · 客户区 {:.0}×{:.0} · 窗口关闭按钮退出",
                size.width,
                size.height
            );
            app.write(&self.status, text);
        }
        // 组件帧同步（`sync` 只在变化时写节点）。
        for button in &self.buttons {
            let _ = button.sync(app);
        }
        for toggle in &self.toggles {
            let _ = toggle.sync(app);
        }
        let _ = self.progress.sync(app, self.window);
        let _ = self.spinner.sync(app);
        let _ = self.tab_bar.sync(app);
        for list in &self.lists {
            let _ = list.sync(app);
        }
        let _ = self.table.sync(app);
        let _ = self.tree.sync(app);
        for input in &self.inputs {
            let _ = input.sync(app);
        }
        for bar in &self.scroll_bars {
            let _ = bar.sync(app);
        }
        let _ = self.dropdown.sync(app);
        let _ = self.select.sync(app);
        let _ = self.menu.sync(app);
        let _ = self.context_menu.sync(app);
        let _ = self.overlay.sync(app);
        let _ = self.tooltip.sync(app);
        let _ = self.dialog.sync(app);
        let _ = self.alert_dialog.sync(app);
        let _ = self.color_picker.sync(app);
    }

    /// 单条事件：日志 / 关窗语义 / 组件 dispatch。
    fn handle_event(&mut self, app: &mut App, event: &Event) {
        match event {
            Event::CloseRequested => {
                app.close_window(self.window);
                return;
            }

            Event::Input(InputEvent::PointerDown { position, .. }) => {
                let hovered = app.hovered(self.window);
                let target = app
                    .window(self.window)
                    .and_then(|target| {
                        hovered.and_then(|node| target.tree().debug_id(node).map(SmolStr::from))
                    })
                    .unwrap_or_else(|| SmolStr::new_static("<none>"));
                log_line(
                    app,
                    &self.log,
                    format_smolstr!(
                        "pointer down ({:.0},{:.0}) -> {target}",
                        position.x,
                        position.y
                    ),
                );
            }
            Event::FocusChanged { focused } => {
                log_line(app, &self.log, format_smolstr!("window: focus={focused}"));
            }
            Event::Resized { size } => {
                log_line(
                    app,
                    &self.log,
                    format_smolstr!("window: resize {:.0}×{:.0}", size.width, size.height),
                );
            }
            Event::DpiChanged { dpi, scale } => {
                log_line(
                    app,
                    &self.log,
                    format_smolstr!("window: DPI {dpi} (scale {:.2})", scale.factor()),
                );
            }
            Event::AppearanceChanged { dark } => {
                let next = if *dark {
                    Appearance::Dark
                } else {
                    Appearance::Light
                };
                app.write(&self.appearance, next);
                self.theme.set_appearance(app, next);
                if let Some(target) = app.window_mut(self.window) {
                    target
                        .set_background_color(self.theme.theme_for(next).role(Role::SurfaceCanvas));
                }
            }
            _ => {}
        }
        // 组件事件转发（按下态 / 键盘 / IME / 指针）。
        for button in &self.buttons {
            button.dispatch(app, event);
        }
        for toggle in &self.toggles {
            toggle.dispatch(app, event);
        }
        self.tab_bar.dispatch(app, event);
        for input in &self.inputs {
            input.dispatch(app, event);
        }
        self.dropdown.dispatch(app, event);
        self.select.dispatch(app, event);
        self.color_picker.dispatch(app, event);
    }
}

/// 建一个按钮（带日志回调；`variant` 决定外观）。
fn make_button(
    app: &mut App,
    theme: &ThemeHandle,
    log: Signal<Vec<SmolStr>>,
    index: usize,
    size: UnifiedSize,
    variant: ButtonVariant,
    disabled: bool,
    loading: bool,
) -> Button {
    let label = match variant {
        ButtonVariant::Primary => SmolStr::new_static("主要"),
        ButtonVariant::Secondary => SmolStr::new_static("次要"),
        ButtonVariant::Outline => SmolStr::new_static("描边"),
        ButtonVariant::Ghost => SmolStr::new_static("幽灵"),
        ButtonVariant::Danger => SmolStr::new_static("危险"),
    };
    let mut button = Button::new(app, theme.clone(), format_smolstr!("gallery.btn.{index}"))
        .with_unified_size(size)
        .label(label.clone())
        .disabled(disabled)
        .loading(loading)
        .on_click({
            move |ctx| {
                log_line(ctx.app_mut(), &log, format_smolstr!("button: {label} 点击"));
            }
        });
    button = match variant {
        ButtonVariant::Primary => button.primary(),
        ButtonVariant::Secondary => button.secondary(),
        ButtonVariant::Outline => button.outline(),
        ButtonVariant::Ghost => button.ghost(),
        ButtonVariant::Danger => button.danger(),
    };
    button
}

/// 建一个声明了自定义动作的演示触发节点（灰底文字；`ActionCtx::node` 即锚点）。
fn demo_trigger(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
    text: &'static str,
    action: Action,
) -> NodeId {
    let theme_style = theme.clone();
    let node = NodeBuilder::boxed()
        .id(format_smolstr!("gallery.trigger.{text}"))
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .justify_center()
                .padding_sides(4.0, 12.0, 4.0, 12.0)
                .height(30.0)
                .build(),
        )
        .paint(PaintBuilder::new().rounded(6.0).build())
        .tab_stop(true)
        .on_click(action)
        .bind_style(move |cx| StylePatch {
            background: Some(theme_style.role(cx, Role::SurfaceControl)),
            border: Some(Border {
                width: 1.0,
                color: theme_style.role(cx, Role::LineBorder),
            }),
            ..StylePatch::default()
        })
        .build(tree(app, window), parent)
        .expect("demo trigger");
    let text_theme = theme.clone();
    NodeBuilder::text(
        text,
        text_style(
            12.0,
            500,
            theme.theme_for(Appearance::Dark).role(Role::FgDefault),
        ),
    )
    .bind_style(move |cx| StylePatch {
        foreground: Some(text_theme.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree(app, window), node)
    .expect("demo trigger text");
    node
}

/// 建一个输入框（安装点击 / 拖拽处理器），返回节点句柄。
#[allow(clippy::too_many_arguments)]
fn make_input(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
    name: &'static str,
    placeholder: &'static str,
    masked: bool,
    disabled: bool,
    multiline: bool,
    size: ControlSize,
) -> InputNode {
    let spec = Arc::new(InputSpec {
        name: SmolStr::new_static(name),
        placeholder: SmolStr::new_static(placeholder),
        masked,
        multiline,
        readonly: false,
        disabled,
        max_length: None,
        size,
        submit: None,
    });
    let state = app.signal(InputState::new(""));
    let placeholder_signal = app.signal(SmolStr::new_static(placeholder));
    let node = input::build(
        tree(app, window),
        parent,
        spec,
        theme.theme_for(Appearance::Dark),
        &state,
        &placeholder_signal,
    );
    node.install(app);
    node
}

/// 打印嵌套虚拟列表当前挂载的行数（`--probe`：验证 page scroll 后行窗口是否重挂）。
fn print_probe_mounted(app: &mut App, prefix: &str, lists: &[List], table: &Table, tree: &Tree) {
    let mut counts: Vec<(&str, usize)> = Vec::with_capacity(lists.len() + 2);
    for (index, list) in lists.iter().enumerate() {
        let name = match index {
            0 => "list_fixed",
            1 => "list_variable",
            _ => "list_deque",
        };
        counts.push((name, list.mounted_rows(app).len()));
    }
    counts.push(("table", table.list().mounted_rows(app).len()));
    counts.push(("tree", tree.list().mounted_rows(app).len()));
    println!("gallery: probe {prefix} mounted_rows {counts:?}");
}

/// 打印一组节点的布局盒（`--probe`：外部驱动点击时用来换算坐标）。
fn print_probe_rects(app: &mut App, window: WindowId, nodes: &[(SmolStr, NodeId)], prefix: &str) {
    for (name, node) in nodes {
        let bounds = app
            .window(window)
            .and_then(|target| target.tree().get(*node).ok())
            .map(|node| node.layout().bounds);
        match bounds {
            Some(bounds) => println!(
                "gallery: probe {prefix}{name} node={node:?} rect=({:.1},{:.1} {:.1}x{:.1})",
                bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height
            ),
            None => println!("gallery: probe {prefix}{name} node={node:?} missing"),
        }
    }
}

/// markdown 解析期的空资源面（行内图片只算尺寸 / 视图层画占位块）。
struct NoAssets;

impl MarkdownAssets for NoAssets {
    fn load(&self, _path: &str) -> Option<Cow<'static, [u8]>> {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 入口
// ─────────────────────────────────────────────────────────────────────────────

/// 真实窗口画廊入口。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flags = parse_flags();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let gallery = Gallery::build(&mut app, flags);
    println!(
        "gallery: window {:?} created ({}×{} logical), 15 sections",
        gallery.window, WINDOW_W, WINDOW_H
    );
    gallery.install(&mut app);
    Application::new().run(app)?;
    println!("gallery: event loop exited cleanly");

    let idle = gallery.idle.borrow();
    let mut failed = false;
    if gallery.flags.idle_check {
        match (idle.pass, idle.frames) {
            (Some(true), frames) => {
                println!("gallery: idle-check PASS (3 s, {frames} frame(s) rendered while idle)");
            }
            (Some(false), frames) => {
                println!(
                    "gallery: idle-check FAIL (3 s, {frames} frames rendered while idle; final frame {:?})",
                    idle.final_frame
                );
                failed = true;
            }
            (None, _) => {
                println!("gallery: idle-check FAIL (report never produced; watchdog closed first)");
                failed = true;
            }
        }
    }
    drop(idle);
    if failed {
        std::process::exit(3);
    }
    Ok(())
}

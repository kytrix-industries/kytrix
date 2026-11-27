use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use image::{Rgba, RgbaImage};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::{App, Application},
    event::{Action, ActionSource, Event, InputEvent, KeyCode, Modifiers, PointerButton},
    interaction::ScrollConfig,
    math::{DevicePixelRatio, DeviceRect, LogicalPoint, LogicalRect, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::Signal,
        tree::{NodeKind, NodeTree},
    },
    paint::{
        color::{Color, ColorResolver},
        primitive::Border,
        style::{PaintSpec, Style, StylePatch},
        style_builder::{PaintBuilder, StyleBuilder},
    },
    test_support::{
        app::TestApp,
        pixels::{
            PersistentPixels, PixelFrame, changed_pixels, channel_span, compare_pixels, pixel,
        },
        record::FrameRecord,
        snapshot::SnapshotImage,
    },
    text::{TextStyle, TextWrap},
    window::{WindowId, WindowOptions},
};
use ui_kit::{
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
        list::{List, ListOptions, SelectionMode},
        markdown::{parse::parse_document, source::MarkdownAssets, view::MarkdownNode},
        menu::{Menu, MenuAction, MenuActivation, MenuEntry, MenuOptions, MenuSubmenu},
        popup_panel::{PanelWidth, PopupPanel, PopupPanelOptions, PopupSurface},
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
    theme::{appearance::Appearance, components::ControlSize, handle::ThemeHandle, role::Role},
};

const WINDOW_W: f32 = 1200.0;
const WINDOW_H: f32 = 800.0;
const HEADER_H: f32 = 62.0;
const SIDE_MARGIN: f32 = 20.0;
const SECTION_W: f32 = WINDOW_W - SIDE_MARGIN * 2.0;
const CONTENT_TOP: f32 = 16.0;
const CONTENT_BOTTOM: f32 = 20.0;
const SECTION_GAP: f32 = 12.0;
const LOG_CAPACITY: usize = 8;

const H_THEME: f32 = 250.0;
const H_BUTTONS: f32 = 210.0;
const H_TOGGLES: f32 = 200.0;
const H_FEEDBACK: f32 = 210.0;
const H_TABS: f32 = 130.0;
const H_LISTS: f32 = 350.0;
const H_SCROLLBAR: f32 = 200.0;
const H_TABLE: f32 = 320.0;
const H_TREE: f32 = 290.0;
const H_INPUTS: f32 = 250.0;
const H_MARKDOWN: f32 = 620.0;
const H_OVERLAYS: f32 = 150.0;
const H_DIALOGS: f32 = 120.0;
const H_PICKER: f32 = 110.0;
const H_LOG: f32 = 240.0;

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

#[derive(Clone, Copy, Default)]
struct Flags {
    auto_demo: bool,
    idle_check: bool,
    theme_check: bool,
    start_light: bool,
    auto_close: Option<u64>,
    probe: bool,
    debug_stack: bool,
    pixel_check: bool,
    real_pixel_check: bool,
    scroll_to: Option<f32>,
}

const GALLERY_USAGE: &str = "gallery: usage: [--auto-demo] [--idle-check] [--theme-check] [--start-light] [--auto-close=<secs>] [--probe] [--debug-stack] [--pixel-check] [--real-pixel-check] [--scroll-to=<px>] [--help]; --pixel-check = headless persistent-incremental pixel gate (exit 0/3); --debug-stack = real-window popup stack/rect gate (exit 0/3); --real-pixel-check = true-window full-screen screenshot pixel gate for ColorPicker/3-level MenuChain/Dialog+scrim (exit 0/3, evidence under target/real-pixels/<os>/); --auto-close is only a watchdog floor (>=30s) under --debug-stack, which exits 0/3 by itself when its final round completes";

fn parse_flags() -> Flags {
    let mut flags = Flags::default();
    for arg in std::env::args().skip(1) {
        if arg == "--auto-demo" {
            flags.auto_demo = true;
        } else if arg == "--idle-check" {
            flags.idle_check = true;
        } else if arg == "--theme-check" {
            flags.theme_check = true;
        } else if arg == "--start-light" {
            flags.start_light = true;
        } else if let Some(value) = arg.strip_prefix("--auto-close=") {
            let Ok(seconds) = value.parse::<u64>() else {
                println!("gallery: invalid --auto-close value {value:?}: expected integer seconds");
                println!("{GALLERY_USAGE}");
                std::process::exit(2);
            };
            flags.auto_close = Some(seconds);
        } else if arg == "--probe" {
            flags.probe = true;
        } else if arg == "--debug-stack" {
            flags.debug_stack = true;
        } else if arg == "--pixel-check" {
            flags.pixel_check = true;
        } else if arg == "--real-pixel-check" {
            flags.real_pixel_check = true;
        } else if let Some(value) = arg.strip_prefix("--scroll-to=") {
            let Ok(offset) = value.parse::<f32>() else {
                println!("gallery: invalid --scroll-to value {value:?}: expected pixel offset");
                println!("{GALLERY_USAGE}");
                std::process::exit(2);
            };
            flags.scroll_to = Some(offset);
        } else if arg == "--help" {
            println!("{GALLERY_USAGE}");
            std::process::exit(0);
        } else if arg == "--dump-overlay" {
            println!("gallery: --dump-overlay was renamed; use --debug-stack");
            std::process::exit(2);
        } else {
            println!("gallery: unknown argument {arg}");
            println!("{GALLERY_USAGE}");
            std::process::exit(2);
        }
    }
    flags
}

#[derive(Debug, Default)]
struct IdleReport {
    baseline: Option<u32>,
    final_frame: Option<u32>,
    frames: u32,
    pass: Option<bool>,
}

#[derive(Debug, Default)]
struct ThemeCheckReport {
    pass: Option<bool>,
    failures: Vec<SmolStr>,
    attempts: u32,
}

#[derive(Debug, Default)]
struct DebugStackReport {
    menu: Option<(usize, f32, f32)>,
    picker_open: bool,
    picker_panel_children: Option<usize>,
    menu_first: Option<LogicalRect>,
    menu_stable: Option<LogicalRect>,
    menu_frames: u32,
    picker_first: Option<LogicalRect>,
    picker_stable: Option<LogicalRect>,
    picker_frames: u32,
    completed: bool,
    failures: Vec<SmolStr>,
}

impl DebugStackReport {
    fn check(&mut self, ok: bool, label: &'static str, detail: SmolStr) {
        if ok {
            return;
        }
        println!("gallery: overlay-check FAIL {label}: {detail}");
        self.failures.push(format_smolstr!("{label}: {detail}"));
    }

    fn menu_metrics_ok(&self) -> bool {
        self.menu
            .is_some_and(|(children, content, thumb)| children > 0 && content > 0.0 && thumb > 0.0)
    }

    fn pass(&self) -> bool {
        self.completed && self.failures.is_empty()
    }

    fn reset_attempt(&mut self) {
        self.menu = None;
        self.picker_open = false;
        self.picker_panel_children = None;
        self.menu_first = None;
        self.menu_stable = None;
        self.menu_frames = 0;
        self.picker_first = None;
        self.picker_stable = None;
        self.picker_frames = 0;
        self.completed = false;
        self.failures.clear();
    }
}

#[derive(Clone)]
struct OverlayCheck {
    window: WindowId,
    menu: Menu,
    chain_menu: Menu,
    color_picker: ColorPicker,
    nodes: Rc<Vec<(SmolStr, NodeId)>>,
    report: Rc<RefCell<DebugStackReport>>,
    generation: Rc<Cell<u32>>,
    retries: Rc<Cell<u32>>,
}

impl OverlayCheck {
    fn new(
        window: WindowId,
        menu: Menu,
        chain_menu: Menu,
        color_picker: ColorPicker,
        nodes: Vec<(SmolStr, NodeId)>,
        report: Rc<RefCell<DebugStackReport>>,
    ) -> Self {
        Self {
            window,
            menu,
            chain_menu,
            color_picker,
            nodes: Rc::new(nodes),
            report,
            generation: Rc::new(Cell::new(0)),
            retries: Rc::new(Cell::new(0)),
        }
    }

    fn start(&self, app: &mut App) {
        self.report.borrow_mut().reset_attempt();
        self.open_menu(app);
        self.schedule(app, 0);
    }

    fn open_menu(&self, app: &mut App) {
        let anchor = self
            .nodes
            .iter()
            .find(|(name, _)| name == "menu")
            .map(|(_, node)| *node)
            .and_then(|node| {
                app.window(self.window)
                    .and_then(|target| target.node_screen_rect(node))
            });
        let opened = match anchor {
            Some(rect) => {
                println!("gallery: overlay-check menu anchored at {rect:?}");
                self.menu
                    .open(app, self.window, ui_core::popup::Anchor::WindowRect(rect))
            }
            None => Ok(false),
        };
        println!("gallery: overlay-check menu.open -> {opened:?}");
        self.report.borrow_mut().check(
            matches!(opened, Ok(true)),
            "menu-open",
            format_smolstr!("menu.open -> {opened:?}"),
        );
        dump_popup_stack(app, self.window, "menu-open");
        app.handle().wake();
    }

    fn schedule(&self, app: &mut App, start: u32) {
        let generation = self.generation.get();
        for round in start..=6 {
            let this = self.clone();
            let delay = Duration::from_millis(u64::from(600 * (round + 1)));
            let _ = app.after(self.window, delay, move |app| {
                if this.generation.get() != generation {
                    return;
                }
                this.round(app, round);
            });
        }
    }

    fn retry(&self, app: &mut App, reason: &'static str) {
        let retries = self.retries.get();
        if retries >= 2 {
            self.report.borrow_mut().check(
                false,
                "environment-dismiss",
                format_smolstr!("环境 dismiss 重试超过上限：{reason}"),
            );
            self.finish(app);
            return;
        }
        self.retries.set(retries + 1);
        self.generation.set(self.generation.get() + 1);
        println!(
            "gallery: overlay-check retry {}（环境 dismiss：{reason}）",
            retries + 1
        );
        self.report.borrow_mut().reset_attempt();
        let _ = self.menu.close_now(app);
        let _ = self.chain_menu.close_now(app);
        let _ = self.color_picker.close(app);
        app.handle().wake();
        let this = self.clone();
        let generation = self.generation.get();
        let _ = app.after(self.window, Duration::from_millis(300), move |app| {
            if this.generation.get() != generation {
                return;
            }
            this.open_menu(app);
            this.schedule(app, 0);
        });
    }

    fn round(&self, app: &mut App, round: u32) {
        let window = self.window;
        let frame = app
            .window(window)
            .map(|target| target.frame_index().get())
            .unwrap_or(0);
        match round {
            0 => {
                if !self.menu.is_open(app) {
                    self.retry(app, "菜单在阶段 0 采样前消失");
                    return;
                }
                let metrics = menu_metrics(app, window);
                println!(
                    "gallery: overlay-check menu round={round} frame={frame} metrics={metrics:?}"
                );
                let entries = stack_entries(app, window);
                let mut state = self.report.borrow_mut();
                record_menu_metrics(&mut state, metrics);
                let metrics_ok = state.menu_metrics_ok();
                let menu_detail = format_smolstr!("菜单度量无效：{:?}", state.menu);
                state.check(metrics_ok, "menu-metrics", menu_detail);
                state.check(
                    entries.len() == 1,
                    "menu-stack-size",
                    format_smolstr!("菜单打开后 popup 栈应有 1 条，实际 {}", entries.len()),
                );
                state.check(
                    entries
                        .first()
                        .is_some_and(|entry| entry.tier == ui_core::popup::PopupTier::Auto),
                    "menu-stack-tier",
                    format_smolstr!("菜单必须是 Auto tier：{entries:?}"),
                );
                drop(state);
                dump_popup_stack(app, window, "menu");
            }
            1 => {
                if !self.menu.is_open(app) {
                    self.retry(app, "菜单在阶段 1 关闭前消失");
                    return;
                }
                let metrics = menu_metrics(app, window);
                let (first, stable) = {
                    let mut state = self.report.borrow_mut();
                    record_menu_metrics(&mut state, metrics);
                    (state.menu_first, state.menu_stable)
                };
                self.report.borrow_mut().check(
                    first.is_some() && first == stable,
                    "menu-first-visible-rect",
                    format_smolstr!("菜单首可见帧 {first:?} != 稳定帧 {stable:?}"),
                );
                let closed = self.menu.close(app);
                println!("gallery: overlay-check menu.close -> {closed}");
                self.report.borrow_mut().check(
                    closed,
                    "menu-close-call",
                    SmolStr::new_static("menu.close 必须接受关闭"),
                );
                let anchor = ui_core::popup::Anchor::WindowRect(LogicalRect::from_origin_size(
                    LogicalPoint::new(px(360.0), px(180.0)),
                    LogicalSize::new(px(1.0), px(1.0)),
                ));
                let root = self.chain_menu.open(app, window, anchor);
                let level2 = self.chain_menu.open_submenu(app, 2).ok().flatten();
                let level3 = level2
                    .as_ref()
                    .and_then(|menu| menu.open_submenu(app, 1).ok().flatten());
                println!(
                    "gallery: overlay-check chain open root={root:?} level2={} level3={}",
                    level2.is_some(),
                    level3.is_some()
                );
                self.report.borrow_mut().check(
                    matches!(root, Ok(true)) && level2.is_some() && level3.is_some(),
                    "chain-open",
                    format_smolstr!(
                        "3 级链打开失败：root={root:?} level2={} level3={}",
                        level2.is_some(),
                        level3.is_some()
                    ),
                );
                dump_popup_stack(app, window, "chain-open");
            }
            2 => {
                if !self.chain_menu.is_open(app) {
                    self.retry(app, "3 级菜单链在阶段 2 采样前消失");
                    return;
                }
                let entries = stack_entries(app, window);
                let mut state = self.report.borrow_mut();
                state.check(
                    entries.len() == 3,
                    "chain-stack-size",
                    format_smolstr!("3 级菜单链应有 3 条 popup，实际 {}", entries.len()),
                );
                state.check(
                    entries.len() == 3
                        && entries[0].z < entries[1].z
                        && entries[1].z < entries[2].z,
                    "chain-z-order",
                    format_smolstr!("最新层必须最上（z 升序）：{entries:?}"),
                );
                state.check(
                    entries
                        .first()
                        .is_some_and(|entry| entry.tier == ui_core::popup::PopupTier::Auto),
                    "chain-tier",
                    format_smolstr!("菜单链必须是 Auto tier：{entries:?}"),
                );
                state.check(
                    entries
                        .get(1)
                        .is_some_and(|entry| entry.parent == entries.first().map(|root| root.id)),
                    "chain-parent-level2",
                    format_smolstr!("二级必须挂在根下：{entries:?}"),
                );
                state.check(
                    entries.get(2).is_some_and(|entry| {
                        entries
                            .get(1)
                            .is_some_and(|level2| entry.parent == Some(level2.id))
                    }),
                    "chain-parent-level3",
                    format_smolstr!("三级必须挂在二级下：{entries:?}"),
                );
                drop(state);
                dump_popup_stack(app, window, "chain");
                let closed = self.chain_menu.close(app);
                println!("gallery: overlay-check chain.close -> {closed}");
                self.report.borrow_mut().check(
                    closed,
                    "chain-close-call",
                    SmolStr::new_static("3 级菜单链必须接受关闭"),
                );
            }
            3 => {
                let entries = stack_entries(app, window);
                self.report.borrow_mut().check(
                    entries.is_empty(),
                    "chain-entries-zero",
                    format_smolstr!("3 级菜单链关闭后 popup 栈必须清空：{entries:?}"),
                );
                dump_popup_stack(app, window, "chain-closed");
                let opened = match self.color_picker.open(app, window) {
                    Ok(opened) => opened,
                    Err(error) => {
                        println!("gallery: overlay-check color-picker.open FAIL {error}");
                        false
                    }
                };
                println!("gallery: overlay-check color-picker.open -> {opened}");
                self.report.borrow_mut().check(
                    opened,
                    "picker-open",
                    SmolStr::new_static("color-picker 必须打开"),
                );
                dump_popup_stack(app, window, "picker-open");
            }
            4 => {
                if !self.color_picker.is_open(app) {
                    self.retry(app, "取色器在阶段 4 采样前消失");
                    return;
                }
                let (is_open, children) = picker_metrics(app, window, &self.color_picker);
                let (first, stable) = {
                    let mut state = self.report.borrow_mut();
                    record_picker_metrics(&mut state, is_open, children);
                    (state.picker_first, state.picker_stable)
                };
                let mut state = self.report.borrow_mut();
                state.check(
                    first.is_some() && first == stable,
                    "picker-first-visible-rect",
                    format_smolstr!("取色器首可见帧 {first:?} != 稳定帧 {stable:?}"),
                );
                let entries = stack_entries(app, window);
                state.check(
                    entries.len() == 1,
                    "picker-stack-size",
                    format_smolstr!("取色器打开后 popup 栈应有 1 条，实际 {}", entries.len()),
                );
                state.check(
                    is_open && children.is_some_and(|count| count > 0),
                    "picker-metrics",
                    format_smolstr!("is_open={is_open} content_children={children:?}"),
                );
                drop(state);
                dump_popup_stack(app, window, "picker");
            }
            5 => {
                if !self.color_picker.is_open(app) {
                    self.retry(app, "取色器在阶段 5 关闭前消失");
                    return;
                }
                let (is_open, children) = picker_metrics(app, window, &self.color_picker);
                record_picker_metrics(&mut self.report.borrow_mut(), is_open, children);
                let closed = self.color_picker.close(app);
                println!("gallery: overlay-check color-picker.close -> {closed}");
                self.report.borrow_mut().check(
                    closed,
                    "picker-close-call",
                    SmolStr::new_static("color-picker.close 必须接受关闭"),
                );
            }
            _ => self.finish(app),
        }
        app.handle().wake();
    }

    fn finish(&self, app: &mut App) {
        let window = self.window;
        let frame = app
            .window(window)
            .map(|target| target.frame_index().get())
            .unwrap_or(0);
        record_stack_empty(app, window, &self.report, "picker-close");
        let entries = stack_entries(app, window);
        let (menu_frames, picker_frames) = {
            let state = self.report.borrow();
            (state.menu_frames, state.picker_frames)
        };
        let mut state = self.report.borrow_mut();
        state.check(
            entries.is_empty(),
            "debug-stack-final",
            format_smolstr!("结束时 popup 栈必须为空：{entries:?}"),
        );
        state.check(
            menu_frames > 0 && picker_frames > 0,
            "debug-stack-frames",
            format_smolstr!(
                "采样帧必须覆盖菜单/取色器打开：menu={menu_frames} picker={picker_frames}"
            ),
        );
        state.completed = true;
        let passed = state.pass();
        println!(
            "gallery: overlay-check final frame={frame} menu_frames={menu_frames} picker_frames={picker_frames} entries={} retries={} {}",
            entries.len(),
            self.retries.get(),
            if passed { "PASS" } else { "FAIL" }
        );
        if !passed {
            println!("gallery: overlay-check detail {:?}", state.failures);
        }
        drop(state);
        dump_popup_stack(app, window, "final");
        println!(
            "gallery: overlay-check complete -> closing window (process exits 0/3 from the report)"
        );
        app.close_window(window);
    }
}

fn switch_appearance(
    app: &mut App,
    window: WindowId,
    theme: &ThemeHandle,
    mirror: &Signal<Appearance>,
    next: Appearance,
) {
    app.write(mirror, next);
    theme.set_appearance(app, next);
    if let Some(target) = app.window_mut(window) {
        target.set_background_color(theme.theme_for(next).role(Role::SurfaceCanvas));
    }
}

fn node_patch(app: &App, window: WindowId, node: NodeId) -> Option<StylePatch> {
    app.window(window)?
        .tree()
        .get(node)
        .ok()
        .map(|node| node.style_patch().clone())
}

fn text_style_color(app: &App, window: WindowId, node: NodeId) -> Option<Color> {
    let target = app.window(window)?;
    match target.tree().get(node).ok()?.kind() {
        NodeKind::Text(content) => Some(content.style.color),
        _ => None,
    }
}

fn text_colors(app: &App, window: WindowId, root: NodeId) -> Vec<(Color, Option<Color>)> {
    let Some(target) = app.window(window) else {
        return Vec::new();
    };
    let tree = target.tree();
    let mut colors = Vec::with_capacity(2);
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let Ok(entry) = tree.get(node) else {
            continue;
        };
        if let NodeKind::Text(content) = entry.kind() {
            colors.push((content.style.color, entry.style_patch().foreground));
        }
        stack.extend(entry.children().iter().rev().copied());
    }
    colors
}

fn check_color(
    failures: &mut Vec<SmolStr>,
    appearance: Appearance,
    label: &str,
    actual: Option<Color>,
    expected: Color,
) {
    let actual = actual.map(|color| ColorResolver.resolve(color).components);
    let expected = ColorResolver.resolve(expected).components;
    if actual != Some(expected) {
        failures.push(format_smolstr!(
            "{appearance:?}/{label}: expected {expected:?} actual {actual:?}"
        ));
    }
}

fn tree(app: &mut App, window: WindowId) -> &mut NodeTree {
    app.window_mut(window)
        .expect("gallery window must be alive")
        .tree_mut()
}

fn text_style(size: f32, weight: u16, color: Color) -> TextStyle {
    TextStyle::new(px(size))
        .with_weight(weight)
        .with_color(color)
}

fn container(app: &mut App, window: WindowId, parent: NodeId, style: Style) -> NodeId {
    NodeBuilder::boxed()
        .style(style)
        .build(tree(app, window), parent)
        .expect("container must have a live parent")
}

fn row(app: &mut App, window: WindowId, parent: NodeId, gap: f32) -> NodeId {
    container(
        app,
        window,
        parent,
        StyleBuilder::new()
            .flex_row()
            .items_center()
            .gap(px(gap))
            .width_full()
            .build(),
    )
}

fn sized_box(app: &mut App, window: WindowId, parent: NodeId, width: f32, height: f32) -> NodeId {
    container(
        app,
        window,
        parent,
        StyleBuilder::new()
            .width(px(width))
            .height(px(height))
            .build(),
    )
}

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
    themed_text(
        tree(app, window),
        parent,
        text,
        text_style(size, weight, initial),
        role,
        theme,
    )
    .expect("themed label must have a live parent")
}

fn themed_text(
    tree: &mut NodeTree,
    parent: NodeId,
    text: impl Into<SmolStr>,
    style: TextStyle,
    role: Role,
    theme: &ThemeHandle,
) -> Result<NodeId, ui_core::error::EngineError> {
    let theme = theme.clone();
    NodeBuilder::text(text, style)
        .bind_style(move |cx| StylePatch {
            foreground: Some(theme.role(cx, role)),
            ..StylePatch::default()
        })
        .build(tree, parent)
}

fn log_line(app: &mut App, log: &Signal<Vec<SmolStr>>, line: impl Into<SmolStr>) {
    let line = line.into();
    app.update(log, |lines| {
        lines.push(line);
        if lines.len() > LOG_CAPACITY {
            lines.remove(0);
        }
    });
}

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
        .width(px(SECTION_W))
        .height(px(height))
        .padding_sides(px(14.0), px(16.0), px(14.0), px(16.0))
        .gap(px(8.0))
        .relative()
        .overflow_hidden()
        .build();
    let card = NodeBuilder::boxed()
        .id(format_smolstr!("gallery.section.{index}"))
        .style(style)
        .paint(PaintBuilder::new().rounded(px(12.0)).build())
        .bind_style({
            let theme = theme.clone();
            move |cx| StylePatch {
                background: Some(theme.role(cx, Role::SurfaceRaised)),
                border: Some(Border {
                    width: px(1.0),
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
            .gap(px(8.0))
            .build(),
    )
}

fn scroll_demo(app: &mut App, window: WindowId, parent: NodeId, theme: &ThemeHandle) -> ScrollBar {
    let initial = theme.theme_for(Appearance::Dark);
    let fg = initial.role(Role::FgMuted);
    let outer = container(
        app,
        window,
        parent,
        StyleBuilder::new()
            .width(px(360.0))
            .height(px(176.0))
            .relative()
            .build(),
    );
    let viewport = container(
        app,
        window,
        outer,
        StyleBuilder::new()
            .width(px(360.0))
            .height(px(176.0))
            .relative()
            .overflow_hidden()
            .build(),
    );
    for index in 0..20usize {
        let row_theme = theme.clone();
        let role = if index % 2 == 0 {
            Role::SurfaceSunken
        } else {
            Role::SurfaceRaised
        };
        let _ = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(
                px(0.0),
                px(index as f32 * 26.0),
                px(360.0),
                px(25.0),
            ))
            .bind_style(move |cx| StylePatch {
                background: Some(row_theme.role(cx, role)),
                ..StylePatch::default()
            })
            .build(tree(app, window), viewport)
            .expect("scroll row");
        let label_theme = theme.clone();
        let _ = NodeBuilder::text(
            format_smolstr!("滚动行 #{index} — 滚轮 / 拖条 / 轨道点击"),
            text_style(12.0, 400, fg),
        )
        .layout_override(LogicalRect::new(
            px(12.0),
            px(index as f32 * 26.0 + 4.0),
            px(320.0),
            px(18.0),
        ))
        .bind_style(move |cx| StylePatch {
            foreground: Some(label_theme.role(cx, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(tree(app, window), viewport)
        .expect("scroll label");
    }
    if let Ok(handle) = app.make_scroll(window, viewport, ScrollConfig::default()) {
        let _ = app.set_scroll_content(&handle, LogicalSize::new(px(360.0), px(20.0 * 26.0)));
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

#[derive(Clone)]
struct Gallery {
    window: WindowId,
    viewport: NodeId,
    content_height: f32,
    theme: ThemeHandle,
    appearance: Signal<Appearance>,
    status: Signal<SmolStr>,
    log: Signal<Vec<SmolStr>>,
    primary_button: Button,
    buttons: Vec<Button>,
    toggles: Vec<Toggle>,
    progress: Progress,
    spinner: Spinner,
    tab_bar: TabBar,
    lists: Vec<List>,
    scroll_bars: Vec<ScrollBar>,
    table: Table,
    tree: Tree,
    inputs: Vec<InputNode>,
    markdown: Rc<RefCell<MarkdownNode>>,
    theme_icons: Vec<NodeId>,
    theme_check: Rc<RefCell<ThemeCheckReport>>,
    debug_stack: Rc<RefCell<DebugStackReport>>,
    dropdown: Dropdown,
    select: Select,
    menu: Menu,
    chain_menu: Menu,
    context_menu: ContextMenu,
    tooltip: Tooltip,
    overlay: PopupPanel,
    dialog: Dialog,
    alert_dialog: Dialog,
    color_picker: ColorPicker,
    flags: Flags,
    idle: Rc<RefCell<IdleReport>>,
    demo_fired: Rc<Cell<bool>>,
    probe_nodes: Vec<(SmolStr, NodeId)>,
}

impl Gallery {
    fn build(app: &mut App, flags: Flags) -> Self {
        let start_appearance = if flags.start_light {
            Appearance::Light
        } else {
            Appearance::Dark
        };
        let theme = ThemeHandle::new(app, start_appearance);
        let initial = theme.theme_for(start_appearance);
        let canvas = initial.role(Role::SurfaceCanvas);
        let fg = initial.role(Role::FgDefault);
        let muted = initial.role(Role::FgMuted);

        let window = app.create_window(WindowOptions {
            title: SmolStr::new_static("Kytrix · ui_kit kit gallery (P2-6b)"),
            size: LogicalSize::new(px(WINDOW_W), px(WINDOW_H)),
            background: canvas,
            ..WindowOptions::default()
        });

        let appearance = app.signal(start_appearance);
        let log = app.signal(Vec::<SmolStr>::new());
        let status = app.signal(SmolStr::new_static("DPI 读取中…"));
        let mut buttons: Vec<Button> = Vec::with_capacity(32);
        let mut chrome_buttons: Vec<Button> = Vec::with_capacity(1);
        let mut primary_button: Option<Button> = None;
        let mut theme_icons: Vec<NodeId> = Vec::with_capacity(3);

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
                    .padding_sides(px(0.0), px(SIDE_MARGIN), px(0.0), px(SIDE_MARGIN))
                    .height(px(HEADER_H))
                    .width_full()
                    .gap(px(14.0))
                    .build(),
            )
            .build(tree(app, window), root)
            .expect("header");
        themed_label(
            app,
            window,
            header,
            "Kytrix · ui_kit kit gallery",
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
                    switch_appearance(app, window, &theme, &appearance, next);
                    log_line(app, &log, format_smolstr!("theme: 切换为 {next:?}"));
                }
            });
        let theme_button_node = theme_button
            .build(app, window, header)
            .expect("theme button");
        chrome_buttons.push(theme_button);

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
                    .height(px(total_height))
                    .padding_sides(
                        px(CONTENT_TOP),
                        px(SIDE_MARGIN),
                        px(CONTENT_BOTTOM),
                        px(SIDE_MARGIN),
                    )
                    .gap(px(SECTION_GAP))
                    .build(),
            )
            .build(tree(app, window), viewport)
            .expect("content");

        let mut toggles: Vec<Toggle> = Vec::with_capacity(10);
        let mut inputs: Vec<InputNode> = Vec::with_capacity(4);
        let mut lists: Vec<List> = Vec::with_capacity(3);
        let mut scroll_bars: Vec<ScrollBar> = Vec::with_capacity(1);

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
                StyleBuilder::new()
                    .flex_col()
                    .width(px(92.0))
                    .gap(px(4.0))
                    .build(),
            );
            let swatch_theme = theme.clone();
            NodeBuilder::boxed()
                .style(StyleBuilder::new().width_full().height(px(34.0)).build())
                .paint(PaintBuilder::new().rounded(px(6.0)).build())
                .bind_style(move |cx| StylePatch {
                    background: Some(swatch_theme.role(cx, role)),
                    border: Some(Border {
                        width: px(1.0),
                        color: swatch_theme.role(cx, Role::LineBorder),
                    }),
                    ..StylePatch::default()
                })
                .build(tree(app, window), wrapper)
                .expect("swatch");
            themed_label(app, window, wrapper, name, 10.5, 500, Role::FgMuted, &theme);
        }
        for chunk in IconKind::ALL.chunks(19) {
            let icons_row = row(app, window, s1, 8.0);
            for kind in chunk {
                let icon = IconSpec::new(*kind)
                    .with_size(px(16.0))
                    .build_themed(app, window, icons_row, &theme, Role::FgMuted)
                    .expect("gallery icon");
                if theme_icons.len() < 3 {
                    theme_icons.push(icon);
                }
            }
        }
        let brand_plate = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(px(16.0))
                    .padding_sides(px(6.0), px(14.0), px(6.0), px(14.0))
                    .height(px(46.0))
                    .build(),
            )
            .paint(PaintSpec {
                background: Color::from_rgba8([0x14, 0x1A, 0x22, 0xFF]),
                corner_radius: [px(6.0); 4],
                ..PaintSpec::default()
            })
            .build(tree(app, window), s1)
            .expect("brand plate");
        for kind in BrandKind::ALL {
            let _ = Brand::new(kind)
                .with_size(px(26.0))
                .with_alt(kind.name())
                .build(app, window, brand_plate);
        }

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
            .with_size(px(18.0))
            .with_role(Role::BrandBase)
            .with_label("静态加载指示（动画默认关闭，保 I3）")
            .spinning(false);
        let _ = spinner.build(app, window, feedback_row);
        let separator_box = sized_box(app, window, feedback_row, 160.0, 20.0);
        let _ = Separator::horizontal(theme.clone()).build(tree(app, window), separator_box);
        let empty_box = sized_box(app, window, feedback_row, 340.0, 36.0);
        let _ = EmptyState::new(theme.clone(), "gallery.empty")
            .text("空态：这里没有内容")
            .build(tree(app, window), empty_box);

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
        let fixed_theme = theme.clone();
        let fixed_list = List::new(
            app,
            theme.clone(),
            "gallery.list.fixed",
            ListOptions::fixed(px(26.0)),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                let theme = fixed_theme.clone();
                NodeBuilder::text(format_smolstr!("等高行 #{index}"), fixed_style.clone())
                    .style(StyleBuilder::new().width_full().build())
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(theme.role(cx, Role::FgMuted)),
                        ..StylePatch::default()
                    })
                    .build(tree, parent)?;
                Ok(())
            },
        );
        let variable_style = text_style(11.5, 400, Color::WHITE).with_wrap(TextWrap::Wrap);
        let variable_theme = theme.clone();
        let variable_list = List::new(
            app,
            theme.clone(),
            "gallery.list.variable",
            ListOptions::variable(px(26.0)),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                let lines = 1 + index % 3;
                let mut text = format_smolstr!("变高行 #{index}：");
                for _ in 0..lines {
                    text = format_smolstr!("{text}测量回写内容 ");
                }
                let theme = variable_theme.clone();
                NodeBuilder::text(text, variable_style.clone())
                    .style(StyleBuilder::new().width_full().build())
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(theme.role(cx, Role::FgDefault)),
                        ..StylePatch::default()
                    })
                    .build(tree, parent)?;
                Ok(())
            },
        );
        let deque_style = text_style(11.5, 400, muted);
        let deque_theme = theme.clone();
        let deque_list = {
            let data = deque_data.clone();
            List::new(
                app,
                theme.clone(),
                "gallery.list.deque",
                ListOptions::fixed(px(26.0)).deque(32),
                move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                    let text = data
                        .borrow()
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| format_smolstr!("deque #{index}"));
                    let theme = deque_theme.clone();
                    NodeBuilder::text(text, deque_style.clone())
                        .style(StyleBuilder::new().width_full().build())
                        .bind_style(move |cx| StylePatch {
                            foreground: Some(theme.role(cx, Role::FgMuted)),
                            ..StylePatch::default()
                        })
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
        let table_theme = theme.clone();
        let table = Table::new(
            app,
            theme.clone(),
            "gallery.table",
            columns,
            TableOptions::variable(px(30.0)).selection(SelectionMode::Multi),
            move |tree: &mut NodeTree, parent: NodeId, row: usize, column: usize| {
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
                let theme = table_theme.clone();
                NodeBuilder::text(text, text_style(11.5, 400, Color::WHITE))
                    .style(StyleBuilder::new().width_full().build())
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(theme.role(cx, Role::FgDefault)),
                        ..StylePatch::default()
                    })
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
        let single_box = sized_box(app, window, input_row, 300.0, 30.0);
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
            None,
            ControlSize::Medium,
        );
        single.update_value(app, "Kytrix 单行输入 · IME 演示");
        let masked_box = sized_box(app, window, input_row, 240.0, 30.0);
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
            None,
            ControlSize::Medium,
        );
        masked.update_value(app, "s3cret-密码");
        let disabled_box = sized_box(app, window, input_row, 240.0, 30.0);
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
            None,
            ControlSize::Medium,
        );
        inputs.extend([single, masked, disabled]);
        let multiline_row = row(app, window, s10, 12.0);
        let multiline_box = sized_box(app, window, multiline_row, 620.0, 58.0);
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
            Some(3),
            ControlSize::Small,
        );
        multiline.update_value(
            app,
            "第一行 🌍 彩色字形\n第二行 👨‍👩‍👧 emoji 走 RGBA 图集，整簇可删可移\n第三行\n第四行\n第五行\n第六行（可视 3 行：滚轮滚动、caret 移动自动露出、右侧 hover 出现滚动条）",
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
        let markdown = MarkdownNode::mount(app, window, s11, theme.clone(), doc)
            .expect("the gallery's markdown mounts");

        let s12 = section(
            app,
            window,
            content,
            12,
            "⑫ 浮层（PopupPanel / Tooltip / Dropdown / Select / Menu / ContextMenu）",
            H_OVERLAYS,
            &theme,
        );
        let overlay_row = row(app, window, s12, 10.0);

        let overlay = PopupPanel::new(
            app,
            theme.clone(),
            "gallery.overlay",
            PopupPanelOptions::new(PopupSurface::menu(), PanelWidth::Auto),
        );
        let overlay_action = Action::Custom(SmolStr::new_static("gallery.overlay.open"));
        let _overlay_trigger = demo_trigger(
            app,
            window,
            overlay_row,
            &theme,
            "打开 Popup",
            overlay_action.clone(),
        );
        app.on_action(overlay_action, {
            let overlay = overlay.clone();
            let panel_theme = theme.clone();
            move |ctx| {
                let node = ctx.node();
                let window = ctx.window();
                let app = ctx.app_mut();
                let theme = panel_theme.clone();
                let spec = ui_core::popup::PopupSpec::new(
                    popup_anchor(node),
                    ui_core::popup::PlacementSpec::default(),
                );
                let opened = overlay.open(app, window, spec, move |app, window, body| {
                    let Some(target) = app.window_mut(window) else {
                        return Err(ui_core::error::EngineError::UnknownWindow { window });
                    };
                    NodeBuilder::text(
                        "Popup 面板（12 向 placement + flip/clamp + 淡入淡出）",
                        text_style(12.0, 400, Color::WHITE),
                    )
                    .style(StyleBuilder::new().padding(px(8.0)).build())
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(theme.role(cx, Role::FgDefault)),
                        ..StylePatch::default()
                    })
                    .build(target.tree_mut(), body)?;
                    Ok(())
                });
                if opened.is_ok() {
                    log_line(app, &log, "popup: 打开（锚点=触发节点）");
                }
            }
        });

        let tooltip = Tooltip::new(
            app,
            theme.clone(),
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

        let dropdown = Dropdown::new(
            app,
            theme.clone(),
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

        let select = Select::new(
            app,
            theme.clone(),
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

        let menu = Menu::new(
            app,
            theme.clone(),
            "gallery.menu",
            [
                MenuEntry::Group(SmolStr::new_static("编辑")),
                MenuEntry::Action(MenuAction::new("copy", "复制").shortcut("Ctrl+C")),
                MenuEntry::Action(MenuAction::new("paste", "粘贴").shortcut("Ctrl+V")),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::new("wrap", "自动换行").checkbox(true)),
                MenuEntry::Action(MenuAction::new("minimap", "显示缩略图").checkbox(false)),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::new("delete", "删除").danger(true)),
                MenuEntry::Submenu(MenuSubmenu::new(
                    "更多",
                    [
                        MenuEntry::Action(MenuAction::new("export", "导出")),
                        MenuEntry::Action(MenuAction::new("settings", "设置").disabled(true)),
                        MenuEntry::Submenu(MenuSubmenu::new(
                            "视图",
                            [
                                MenuEntry::Action(MenuAction::new("light", "浅色").radio(true)),
                                MenuEntry::Action(MenuAction::new("dark", "深色").radio(false)),
                            ],
                        )),
                    ],
                )),
            ],
            MenuOptions::default(),
        )
        .on_activate({
            move |app, activation| {
                log_line(
                    app,
                    &log,
                    match activation.check {
                        Some(check) => {
                            format_smolstr!("menu: 激活 {} / 勾选={}", activation.id, check.checked)
                        }
                        None => format_smolstr!("menu: 激活 {}", activation.id),
                    },
                );
            }
        });
        let menu_action = Action::Custom(SmolStr::new_static("gallery.menu.open"));
        let menu_trigger = demo_trigger(
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
                let opened = menu.open(app, window, popup_anchor(node));
                if opened.is_ok() {
                    log_line(app, &log, "menu: 打开（↑↓ / Enter / Esc / 子菜单 →）");
                }
            }
        });

        let chain_menu = Menu::new(
            app,
            theme.clone(),
            "gallery.menu-chain",
            [
                MenuEntry::Action(MenuAction::new("first", "第一项").checkbox(true)),
                MenuEntry::Action(MenuAction::new("second", "第二项")),
                MenuEntry::Submenu(MenuSubmenu::new(
                    "二级",
                    [
                        MenuEntry::Action(MenuAction::new("inner", "内部")),
                        MenuEntry::Submenu(MenuSubmenu::new(
                            "三级",
                            [
                                MenuEntry::Action(MenuAction::new("third", "第三项").radio(true)),
                                MenuEntry::Action(
                                    MenuAction::new("fourth", "第四项").disabled(true),
                                ),
                            ],
                        )),
                    ],
                )),
            ],
            MenuOptions::default(),
        )
        .on_activate({
            move |app, activation| {
                log_line(
                    app,
                    &log,
                    format_smolstr!("menu-chain: 激活 {}", activation.id),
                );
            }
        });

        let context_menu = ContextMenu::new(
            app,
            theme.clone(),
            "gallery.context-menu",
            [
                MenuEntry::Action(MenuAction::new("inspect", "查看")),
                MenuEntry::Action(MenuAction::new("duplicate", "复制一份")),
                MenuEntry::Separator,
                MenuEntry::Submenu(MenuSubmenu::new(
                    "变换",
                    [
                        MenuEntry::Action(MenuAction::new("flip", "翻转")),
                        MenuEntry::Action(MenuAction::new("rotate", "旋转 90°").disabled(true)),
                    ],
                )),
                MenuEntry::Action(MenuAction::new("remove", "移除").danger(true)),
            ],
            ContextMenuOptions::default(),
        )
        .on_item_click({
            move |app, id, check| {
                log_line(
                    app,
                    &log,
                    match check {
                        Some(check) => {
                            format_smolstr!("context-menu: 点击 {id} / 勾选={}", check.checked)
                        }
                        None => format_smolstr!("context-menu: 点击 {id}"),
                    },
                )
            }
        });
        let context_action = Action::Custom(SmolStr::new_static("gallery.context.open"));
        let context_trigger = demo_trigger(
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
                    .and_then(|target| target.node_screen_rect(node))
                    .map(|bounds| LogicalPoint::new(bounds.origin.x + 24.0, bounds.max().y + 6.0))
                    .unwrap_or(LogicalPoint::new(px(420.0), px(600.0)));
                let opened = context_menu.open(app, window, point);
                if opened.is_ok() {
                    log_line(app, &log, "context-menu: 打开（按钮演示右键语义）");
                }
            }
        });

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
        let dialog_theme = theme.clone();
        let dialog = DialogBuilder::new("示例对话框")
            .content(move |app, window, body| {
                let theme = dialog_theme.clone();
                NodeBuilder::text(
                    "模态对话框：Esc 关闭 / 点遮罩关闭 / 焦点陷阱与恢复。",
                    text_style(12.5, 400, Color::WHITE).with_wrap(TextWrap::Wrap),
                )
                .style(StyleBuilder::new().width_full().build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme.role(cx, Role::FgDefault)),
                    ..StylePatch::default()
                })
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
            .build(app, theme.clone(), "gallery.dialog");
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
        .build(app, theme.clone(), "gallery.alert");
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
            "gallery.color-picker",
            Color::from_rgba8([0x4C, 0x9A, 0xFF, 0xFF]),
            UnifiedSize::Medium,
        );
        color_picker.on_change(move |app, color| {
            log_line(app, &log, format_smolstr!("color-picker: {color:?}"))
        });

        let _ = color_picker.build(app, window, picker_row);

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
        let log_theme = theme.clone();
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
        .bind_style(move |cx| StylePatch {
            foreground: Some(log_theme.role(cx, Role::FgDefault)),
            ..StylePatch::default()
        })
        .style(StyleBuilder::new().width_full().flex_1().build())
        .build(tree(app, window), s15)
        .expect("log text");

        match app.make_scroll(window, viewport, ScrollConfig::default()) {
            Ok(handle) => {
                let _ = app
                    .set_scroll_content(&handle, LogicalSize::new(px(WINDOW_W), px(total_height)));
            }
            Err(error) => println!("gallery: WARN viewport scroll registration failed: {error}"),
        }

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
        probe_nodes.push((SmolStr::new_static("menu"), menu_trigger));
        probe_nodes.push((SmolStr::new_static("context"), context_trigger));
        if let Some(node) = dialog.confirm_button().and_then(|button| button.node()) {
            probe_nodes.push((SmolStr::new_static("dialog_confirm"), node));
        }
        for (index, input) in inputs.iter().enumerate() {
            probe_nodes.push((format_smolstr!("input{index}"), input.editable));
        }
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
            markdown: Rc::new(RefCell::new(markdown)),
            theme_icons,
            theme_check: Rc::new(RefCell::new(ThemeCheckReport::default())),
            debug_stack: Rc::new(RefCell::new(DebugStackReport::default())),
            dropdown,
            select,
            menu,
            chain_menu,
            context_menu,
            tooltip,
            overlay,
            dialog,
            alert_dialog,
            color_picker,
            flags,
            idle: Rc::new(RefCell::new(IdleReport::default())),
            demo_fired: Rc::new(Cell::new(false)),
            probe_nodes,
        }
    }

    fn install(&self, app: &mut App) {
        let mut hook = self.clone();
        app.on_frame(move |app| hook.tick(app));

        let window = self.window;
        if self.flags.theme_check {
            let gallery = self.clone();
            let _ = app.after(window, Duration::from_millis(900), move |app| {
                gallery.start_theme_check(app, window);
            });
        }
        if self.flags.probe {
            let gallery = self.clone();
            let nodes = self.probe_nodes.clone();
            let viewport = self.viewport;
            let content_height = self.content_height;
            let lists = self.lists.clone();
            let table = self.table.clone();
            let tree = self.tree.clone();
            let inputs = self.inputs.clone();
            let _ = app.after(window, Duration::from_millis(1_500), move |app| {
                println!("gallery: probe content_height={content_height}");
                print_probe_rects(app, window, &nodes, "");
                print_probe_mounted(app, "top", &lists, &table, &tree);
                print_input_probe(app, window, &inputs, "top");
                if let Some(handle) = app.scroll_handle(window, viewport) {
                    let _ = app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(1.0e6)));
                }
                let nodes = nodes.clone();
                let lists = lists.clone();
                let table = table.clone();
                let tree = tree.clone();
                let inputs = inputs.clone();
                let _ = app.after(window, Duration::from_millis(600), move |app| {
                    if let Some(handle) = app.scroll_handle(window, viewport) {
                        let offset = app
                            .scroll_state(&handle)
                            .map(|state| state.offset.y)
                            .unwrap_or(px(0.0));
                        println!("gallery: probe bottom scroll_offset={offset}");
                    }
                    print_probe_rects(app, window, &nodes, "bottom ");
                    print_probe_mounted(app, "bottom", &lists, &table, &tree);
                    print_input_probe(app, window, &inputs, "bottom ");
                    if let Some(handle) = app.scroll_handle(window, viewport) {
                        let _ = app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(2_300.0)));
                    }
                    if let Some((_, node)) = nodes.iter().find(|(name, _)| name == "input0") {
                        let _ = app.set_focus(window, Some(*node));
                        println!("gallery: probe focused first input node={node:?}");
                        print_input_probe(app, window, &inputs, "focused ");
                        if let Some(multiline) = inputs.get(3) {
                            let _ = app.set_focus(window, Some(multiline.editable));
                            println!(
                                "gallery: probe focused multiline node={:?}",
                                multiline.editable
                            );
                            print_input_probe(app, window, &inputs, "multiline ");
                            multiline_wrap_probe(app, window, multiline, "multiline ");
                        }
                        let gallery = gallery.clone();
                        let _ = app.after(window, Duration::from_millis(500), move |app| {
                            gallery.probe_multiline_wrap(app, window);
                        });
                        let _ = app.after(window, Duration::from_millis(600), move |app| {
                            print_input_probe(app, window, &inputs, "blink ");
                        });
                    }
                });
            });
        }
        if self.flags.debug_stack {
            let viewport = self.viewport;
            let menu = self.menu.clone();
            let chain_menu = self.chain_menu.clone();
            let color_picker = self.color_picker.clone();
            let nodes = self.probe_nodes.clone();
            let report = self.debug_stack.clone();
            let _ = app.after(window, Duration::from_millis(600), move |app| {
                if let Some(handle) = app.scroll_handle(window, viewport) {
                    let _ = app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(3_150.0)));
                }
                print_probe_rects(app, window, &nodes, "debug-stack ");
                app.handle().wake();
                let check = OverlayCheck::new(
                    window,
                    menu.clone(),
                    chain_menu.clone(),
                    color_picker.clone(),
                    nodes.clone(),
                    report.clone(),
                );
                let _ = app.after(window, Duration::from_millis(300), move |app| {
                    check.start(app);
                });
            });
        }
        if let Some(offset) = self.flags.scroll_to {
            let viewport = self.viewport;
            let _ = app.after(window, Duration::from_millis(8_000), move |app| {
                if let Some(handle) = app.scroll_handle(window, viewport) {
                    let _ = app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(offset)));
                    println!("gallery: scroll-to offset={offset}");
                }
                app.handle().wake();
            });
        }
        if self.flags.auto_demo {
            let button = self.primary_button.clone();
            let log = self.log;
            let demo = self.demo_fired.clone();
            let _ = app.after(window, Duration::from_millis(1_200), move |app| {
                let before = app
                    .window(window)
                    .map(|target| target.frame_index().get())
                    .unwrap_or(0);
                demo.set(true);
                app.handle().wake();
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
            && !self.flags.debug_stack
        {
            let _ = app.after(window, Duration::from_secs(seconds), move |app| {
                println!("gallery: auto-close after {seconds}s");
                app.close_window(window);
                app.handle().wake();
            });
        }
        if self.flags.debug_stack {
            let seconds = self.flags.auto_close.map_or(30, |seconds| seconds.max(30));
            let report = self.debug_stack.clone();
            let _ = app.after(window, Duration::from_secs(seconds), move |app| {
                println!(
                    "gallery: overlay-check watchdog after {seconds}s (automation did not finish)"
                );
                let mut report = report.borrow_mut();
                if !report.completed {
                    report.check(
                        false,
                        "watchdog-timeout",
                        format_smolstr!(
                            "--debug-stack automation did not finish within {seconds}s"
                        ),
                    );
                }
                drop(report);
                app.close_window(window);
                app.handle().wake();
            });
        }
    }

    fn probe_multiline_wrap(&self, app: &mut App, window: WindowId) {
        let Some(target) = app.window(window) else {
            return;
        };
        let root = target.tree().root();
        let host = {
            let tree = app.window_mut(window).expect("window exists").tree_mut();
            NodeBuilder::boxed()
                .id("gallery.wrap-probe.host")
                .style(
                    StyleBuilder::new()
                        .width(px(160.0))
                        .height(px(60.0))
                        .build(),
                )
                .build(tree, root)
                .expect("wrap probe host")
        };
        let input = make_input(
            app,
            window,
            host,
            &self.theme,
            "gallery.input.wrap-probe",
            "",
            false,
            false,
            true,
            Some(3),
            ControlSize::Small,
        );
        input.update_value(app, "。".repeat(40).as_str());
        let _ = input.sync(app);
        app.handle().wake();
        let _ = app.after(window, Duration::from_millis(800), move |app| {
            let _ = input.sync(app);
            multiline_wrap_probe(app, window, &input, "wrap-probe-40 ");
            app.handle().wake();
        });
    }

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

        if let Some(target) = app.window(self.window) {
            let size = target.size();
            let text = format_smolstr!(
                "客户区 {:.0}×{:.0} · 窗口关闭按钮退出",
                size.width,
                size.height
            );
            let text = if self.demo_fired.get() {
                format_smolstr!("{text} · auto-demo ✓")
            } else {
                text
            };
            app.write(&self.status, text);
        }
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
        if let Ok(mut markdown) = self.markdown.try_borrow_mut() {
            markdown.sync(app);
        }
        for bar in &self.scroll_bars {
            let _ = bar.sync(app);
        }
        let _ = self.dropdown.sync(app);
        let _ = self.select.sync(app);
        let _ = self.menu.sync(app);
        let _ = self.chain_menu.sync(app);
        let _ = self.context_menu.sync(app);
        let _ = self.overlay.sync(app);
        let _ = self.tooltip.sync(app);
        let _ = self.dialog.sync(app);
        let _ = self.alert_dialog.sync(app);
        let _ = self.color_picker.sync(app);
        if self.flags.debug_stack {
            self.capture_debug_stack(app);
        }
    }

    fn capture_debug_stack(&self, app: &App) {
        let mut report = self.debug_stack.borrow_mut();
        if let Some(node) = self.menu.popup_node()
            && let Some(entry) = stack_entries(app, self.window)
                .into_iter()
                .find(|entry| entry.node == node)
            && entry.state == ui_core::popup::PopupState::Open
            && !entry.hit_rect.is_empty()
        {
            report.menu_frames += 1;
            report.menu_first.get_or_insert(entry.hit_rect);
            report.menu_stable = Some(entry.hit_rect);
        }
        if let Some(node) = self.color_picker.popup_node()
            && let Some(entry) = stack_entries(app, self.window)
                .into_iter()
                .find(|entry| entry.node == node)
            && entry.state == ui_core::popup::PopupState::Open
            && !entry.hit_rect.is_empty()
        {
            report.picker_frames += 1;
            report.picker_first.get_or_insert(entry.hit_rect);
            report.picker_stable = Some(entry.hit_rect);
        }
    }

    fn start_theme_check(&self, app: &mut App, window: WindowId) {
        let frames = app
            .window(window)
            .map(|target| target.frame_index().get())
            .unwrap_or(0);
        if frames == 0 {
            let attempts = {
                let mut report = self.theme_check.borrow_mut();
                report.attempts += 1;
                report.attempts
            };
            if attempts > 20 {
                println!("gallery: theme-check FAIL (no frame rendered within ~4 s)");
                let mut report = self.theme_check.borrow_mut();
                report.pass = Some(false);
                drop(report);
                app.close_window(window);
                app.handle().wake();
                return;
            }
            let gallery = self.clone();
            let _ = app.after(window, Duration::from_millis(200), move |app| {
                gallery.start_theme_check(app, window);
            });
            app.handle().wake();
            return;
        }
        let start = self.theme.current(app);
        let other = match start {
            Appearance::Dark => Appearance::Light,
            Appearance::Light => Appearance::Dark,
        };
        let start_failures = self.verify_theme(app, window, start);
        println!(
            "gallery: theme-check {start:?} failures={}",
            start_failures.len()
        );
        switch_appearance(app, window, &self.theme, &self.appearance, other);
        let gallery = self.clone();
        let _ = app.after(window, Duration::from_millis(400), move |app| {
            let other_failures = gallery.verify_theme(app, window, other);
            println!(
                "gallery: theme-check {other:?} failures={}",
                other_failures.len()
            );
            switch_appearance(app, window, &gallery.theme, &gallery.appearance, start);
            let mut failures = start_failures.clone();
            failures.extend(other_failures);
            let gallery = gallery.clone();
            let _ = app.after(window, Duration::from_millis(400), move |app| {
                let back = gallery.verify_theme(app, window, start);
                println!(
                    "gallery: theme-check {start:?}-again failures={}",
                    back.len()
                );
                failures.extend(back);
                let pass = failures.is_empty();
                for failure in &failures {
                    println!("gallery: theme-check FAIL {failure}");
                }
                println!(
                    "gallery: theme-check {} ({} failure(s) across {start:?}/{other:?}/{start:?})",
                    if pass { "PASS" } else { "FAIL" },
                    failures.len()
                );
                let mut report = gallery.theme_check.borrow_mut();
                report.pass = Some(pass);
                report.failures = failures;
                drop(report);
                app.close_window(window);
                app.handle().wake();
            });
            app.handle().wake();
        });
        app.handle().wake();
    }

    fn verify_theme(&self, app: &App, window: WindowId, appearance: Appearance) -> Vec<SmolStr> {
        let mut failures: Vec<SmolStr> = Vec::with_capacity(10);
        let theme = self.theme.theme_for(appearance);
        let input = theme.components().input;
        let default_fg = theme.role(Role::FgDefault);
        let muted = theme.role(Role::FgMuted);

        if let Some(built) = self.inputs.first() {
            let root = node_patch(app, window, built.root);
            check_color(
                &mut failures,
                appearance,
                "input.root.background",
                root.as_ref().and_then(|patch| patch.background),
                input.background.enabled,
            );
            check_color(
                &mut failures,
                appearance,
                "input.root.border",
                root.as_ref()
                    .and_then(|patch| patch.border)
                    .map(|border| border.color),
                input.border.normal,
            );
            check_color(
                &mut failures,
                appearance,
                "input.text.color",
                text_style_color(app, window, built.editable),
                if built.spec.disabled {
                    input.foreground.disabled
                } else {
                    input.foreground.enabled
                },
            );
            check_color(
                &mut failures,
                appearance,
                "input.placeholder.color",
                app.text_edit_placeholder_color(window, built.editable).ok(),
                input.placeholder,
            );
        }
        for (index, built) in self.inputs.iter().enumerate() {
            if !built.spec.disabled {
                continue;
            }
            let label = format_smolstr!("input{index}.disabled.background");
            check_color(
                &mut failures,
                appearance,
                &label,
                node_patch(app, window, built.root).and_then(|patch| patch.background),
                input.background.disabled,
            );
        }

        for (index, node) in self.theme_icons.iter().enumerate() {
            let label = format_smolstr!("icon{index}.foreground");
            check_color(
                &mut failures,
                appearance,
                &label,
                node_patch(app, window, *node).and_then(|patch| patch.foreground),
                muted,
            );
        }

        if let Ok(markdown) = self.markdown.try_borrow() {
            let colors = text_colors(app, window, markdown.root());
            check_color(
                &mut failures,
                appearance,
                "markdown.text.color",
                colors.first().map(|(color, _)| *color),
                default_fg,
            );
        }

        if let Some(row) = self
            .lists
            .first()
            .and_then(|list| list.row_content_node(app, 0))
        {
            let colors = text_colors(app, window, row);
            check_color(
                &mut failures,
                appearance,
                "list.row.foreground",
                colors.first().and_then(|(_, patch)| *patch),
                muted,
            );
        }

        if let Some(row) = self.tree.list().row_content_node(app, 0) {
            let colors = text_colors(app, window, row);
            check_color(
                &mut failures,
                appearance,
                "tree.row.caret",
                colors.first().and_then(|(_, patch)| *patch),
                muted,
            );
            check_color(
                &mut failures,
                appearance,
                "tree.row.label",
                colors.last().and_then(|(_, patch)| *patch),
                default_fg,
            );
        }

        failures
    }

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
            Event::DpiChanged { dpi } => {
                log_line(app, &self.log, format_smolstr!("window: DPI {dpi}"));
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
        for button in &self.buttons {
            button.dispatch(app, event);
        }
        for toggle in &self.toggles {
            toggle.dispatch(app, event);
        }
        self.tab_bar.dispatch(app, event);
        self.dropdown.dispatch(app, event);
        self.select.dispatch(app, event);
    }
}

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

fn popup_anchor(node: NodeId) -> ui_core::popup::Anchor {
    ui_core::popup::Anchor::Node { node }
}

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
                .padding_sides(px(4.0), px(12.0), px(4.0), px(12.0))
                .height(px(30.0))
                .build(),
        )
        .paint(PaintBuilder::new().rounded(px(6.0)).build())
        .tab_stop(true)
        .on_click(action)
        .bind_style(move |cx| StylePatch {
            background: Some(theme_style.role(cx, Role::SurfaceControl)),
            border: Some(Border {
                width: px(1.0),
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
    visible_lines: Option<u16>,
    size: ControlSize,
) -> InputNode {
    let spec = Arc::new(InputSpec {
        name: SmolStr::new_static(name),
        label: None,
        placeholder: SmolStr::new_static(placeholder),
        masked,
        multiline,
        readonly: false,
        disabled,
        max_length: None,
        visible_lines,
        size,
        submit: None,
    });
    let state = app.signal(InputState::new(""));
    let placeholder_signal = app.signal(SmolStr::new_static(placeholder));
    let node = input::build(
        tree(app, window),
        parent,
        spec,
        theme,
        &state,
        &placeholder_signal,
    );
    node.install(app);
    node
}

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

fn multiline_wrap_probe(app: &App, window: WindowId, input: &InputNode, prefix: &str) {
    let Some(target) = app.window(window) else {
        return;
    };
    let Some(value) = app.text_edit_value(window, input.editable).ok() else {
        return;
    };
    let Ok(metrics) = app.text_edit_metrics(window, input.editable) else {
        return;
    };
    let editable = target
        .tree()
        .get(input.editable)
        .ok()
        .map(|node| node.layout().bounds);
    let mut lines: Vec<(f32, f32)> = Vec::new();
    for byte in 0..=value.len() {
        if !value.is_char_boundary(byte) {
            continue;
        }
        let Ok(caret) = target.text_caret_rect(input.editable, byte) else {
            continue;
        };
        let y = caret.origin.y.value();
        let x = caret.origin.x.value();
        match lines.iter_mut().find(|(line_y, _)| *line_y == y) {
            Some((_, right)) => *right = right.max(x),
            None => lines.push((y, x)),
        }
    }
    let widest = lines.iter().map(|(_, right)| *right).fold(0.0f32, f32::max);
    let scroll = app
        .scroll_handle(window, input.viewport)
        .and_then(|handle| app.scroll_state(&handle))
        .map(|state| {
            (
                state.offset.x.value(),
                state.offset.y.value(),
                state.content.height.value(),
                state.viewport.height.value(),
            )
        });
    let scrollable = input
        .scrollbar
        .borrow()
        .as_ref()
        .map(|bar| bar.scrollable(app));
    println!(
        "gallery: wrap-probe {prefix}chars={} lines={} content_height={} line_height={} per_line_right={lines:?} widest_right={widest} editable_width={:?} horizontal_overflow={} scroll(x,y,content,viewport)={scroll:?} scrollable={scrollable:?}",
        value.chars().count(),
        metrics.line_count,
        metrics.content_height.value(),
        metrics.line_height.value(),
        editable.map(|bounds| bounds.size.width.value()),
        editable.is_some_and(|bounds| widest > bounds.size.width.value() + 1.0),
    );
}

fn print_input_probe(app: &App, window: WindowId, inputs: &[InputNode], prefix: &str) {
    let Some(target) = app.window(window) else {
        return;
    };
    let tree = target.tree();
    println!(
        "gallery: probe {prefix}caret_visible={:?} next_wake={:?}",
        app.text_edit_caret_visible(window),
        app.next_wake_deadline()
    );
    for (index, input) in inputs.iter().enumerate() {
        let root = tree.get(input.root).ok().map(|node| node.layout().bounds);
        let editable = tree
            .get(input.editable)
            .ok()
            .map(|node| node.layout().bounds);
        let metrics = app.text_edit_metrics(window, input.editable).ok();
        let snapshot = app.text_edit_snapshot(window, input.editable).ok();
        let caret = snapshot.as_ref().and_then(|snapshot| {
            target
                .text_caret_rect(input.editable, snapshot.selection.focus().byte)
                .ok()
        });
        let value = app.text_edit_value(window, input.editable).ok();
        let scrollbar = input
            .scrollbar
            .borrow()
            .as_ref()
            .map(|bar| (bar.scrollable(app), bar.thumb_rect(app), bar.opacity(app)));
        let scroll = app
            .scroll_handle(window, input.viewport)
            .and_then(|handle| app.scroll_state(&handle))
            .map(|state| (state.axis, state.offset, state.viewport, state.content));
        println!(
            "gallery: probe {prefix}input{index} root={root:?} editable={editable:?} metrics={metrics:?} caret={caret:?} selection={:?} composing={:?} focused={:?} scroll={scroll:?} scrollbar={scrollbar:?} value={value:?}",
            snapshot.as_ref().map(|snapshot| snapshot.selection.range()),
            snapshot.as_ref().map(|snapshot| snapshot.composing),
            snapshot.as_ref().map(|snapshot| snapshot.focused),
        );
    }
}

fn stack_entries(app: &App, window: WindowId) -> Vec<ui_core::popup::PopupSnapshotEntry> {
    app.popup_stack(window)
        .map_or_else(Vec::new, |stack| stack.entries)
}

fn record_stack_empty(
    app: &App,
    window: WindowId,
    report: &Rc<RefCell<DebugStackReport>>,
    label: &'static str,
) {
    let entries = stack_entries(app, window);
    report.borrow_mut().check(
        entries.is_empty(),
        label,
        format_smolstr!(
            "关闭后 popup 栈仍有 {} 条（不得残留）：{entries:?}",
            entries.len()
        ),
    );
}

fn dump_popup_stack(app: &App, window: WindowId, label: &str) {
    let Some(stack) = app.popup_stack(window) else {
        println!("gallery: debug-stack {label} window={window:?} <missing window>");
        return;
    };
    println!(
        "gallery: debug-stack {label} window={window:?} root={:?} entries={} z_order={:?}",
        stack.root,
        stack.entries.len(),
        stack
            .entries
            .iter()
            .map(|entry| entry.z)
            .collect::<Vec<_>>()
    );
    for entry in &stack.entries {
        let a11y_rect = app
            .popup_rect(entry.id)
            .map_or(entry.hit_rect, |rect| rect.a11y);
        println!(
            "gallery: debug-stack   id={:?} tier={:?} parent={:?} z={} state={:?} hit=({:.1},{:.1} {:.1}x{:.1}) paint=({:.1},{:.1} {:.1}x{:.1}) a11y=({:.1},{:.1} {:.1}x{:.1}) owner_modal={:?} focus={:?} node={:?} paint_offset=({:.1},{:.1}) constrained={:?}",
            entry.id,
            entry.tier,
            entry.parent,
            entry.z,
            entry.state,
            entry.hit_rect.origin.x,
            entry.hit_rect.origin.y,
            entry.hit_rect.size.width,
            entry.hit_rect.size.height,
            entry.paint_rect.origin.x,
            entry.paint_rect.origin.y,
            entry.paint_rect.size.width,
            entry.paint_rect.size.height,
            a11y_rect.origin.x,
            a11y_rect.origin.y,
            a11y_rect.size.width,
            a11y_rect.size.height,
            entry.owner_modal,
            entry.focus,
            entry.node,
            entry.paint_offset.x,
            entry.paint_offset.y,
            entry.constrained_size,
        );
    }
}

fn find_debug_id(app: &App, window: WindowId, debug_id: &str) -> Option<NodeId> {
    let tree = app.window(window)?.tree();
    let mut stack = vec![tree.root()];
    while let Some(node) = stack.pop() {
        if tree.debug_id(node) == Some(debug_id) {
            return Some(node);
        }
        if let Ok(entry) = tree.get(node) {
            stack.extend(entry.children().iter().rev().copied());
        }
    }
    None
}

fn menu_metrics(app: &App, window: WindowId) -> Option<(usize, f32, f32)> {
    let viewport = find_debug_id(app, window, "gallery.menu:panel:list:viewport")?;
    let children = {
        let tree = app.window(window)?.tree();
        tree.get(viewport).ok()?.children().len()
    };
    let handle = app.scroll_handle(window, viewport)?;
    let content = app.scroll_state(&handle)?.content.height;
    let thumb = find_debug_id(app, window, "gallery.menu:panel:list:scrollbar:thumb")?;
    let thumb_height = {
        let tree = app.window(window)?.tree();
        tree.get(thumb).ok()?.layout().bounds.size.height
    };
    Some((children, f32::from(content), f32::from(thumb_height)))
}

fn picker_metrics(app: &App, window: WindowId, picker: &ColorPicker) -> (bool, Option<usize>) {
    let open = picker.is_open(app) && *app.read(&picker.open_state());
    let children =
        find_debug_id(app, window, "gallery.color-picker:panel.content").and_then(|node| {
            let tree = app.window(window)?.tree();
            Some(tree.get(node).ok()?.children().len())
        });
    (open, children)
}

fn record_menu_metrics(report: &mut DebugStackReport, metrics: Option<(usize, f32, f32)>) {
    let Some(metrics) = metrics else {
        return;
    };
    report.menu = Some(match report.menu {
        Some((children, content, thumb)) => (
            children.max(metrics.0),
            content.max(metrics.1),
            thumb.max(metrics.2),
        ),
        None => metrics,
    });
}

fn record_picker_metrics(report: &mut DebugStackReport, is_open: bool, children: Option<usize>) {
    report.picker_open |= is_open;
    if children.is_some_and(|count| count > 0) {
        report.picker_panel_children = children;
    }
}

struct NoAssets;

impl MarkdownAssets for NoAssets {
    fn load(&self, _path: &str) -> Option<Cow<'static, [u8]>> {
        None
    }
}

const PIXEL_CHECK_POPUP_MARGIN: f32 = 16.0;
const PIXEL_CHECK_TOLERANCE: u8 = 2;
const PIXEL_CHECK_SETTLE_LIMIT: usize = 40;
const PIXEL_CHECK_IDLE_FRAMES: usize = 20;
const PIXEL_CHECK_IDLE_STEP_MS: u64 = 137;
const PIXEL_CHECK_FRAME_STEP_MS: u64 = 16;
const PIXEL_CHECK_MIN_COMPARED_PERMILLE: u64 = 30;

#[derive(Default)]
struct PixelCheckReport {
    checks: u32,
    failures: u32,
    seen: Vec<(&'static str, u32)>,
    damage_full_frames: u32,
    sampled_frames: u32,
    open_animation_frames: u32,
    close_animation_frames: [u32; 2],
    first_visible: Option<LogicalRect>,
    stable: Option<LogicalRect>,
    moved: Option<LogicalRect>,
}

impl PixelCheckReport {
    fn record(&mut self, ok: bool, key: &'static str, message: SmolStr) {
        self.checks += 1;
        if ok {
            return;
        }
        self.failures += 1;
        if let Some(entry) = self.seen.iter_mut().find(|(seen, _)| *seen == key) {
            entry.1 += 1;
            return;
        }
        self.seen.push((key, 1));
        println!("gallery: pixel-check FAIL {message}");
    }
}

struct PixelCheckRun {
    report: PixelCheckReport,
    before: SnapshotImage,
    previous: SnapshotImage,
}

impl PixelCheckRun {
    fn observe(
        &mut self,
        app: &TestApp,
        picker: &ColorPicker,
        pixels: &mut PersistentPixels,
        record: &FrameRecord,
        stage: &'static str,
        compare_background: bool,
    ) {
        let popup = pixel_check_popup(app, picker);
        if self.report.first_visible.is_none()
            && picker.popup_state(app.app()) == Some(ui_core::popup::PopupState::Open)
            && popup.is_some_and(|rect| !rect.is_empty())
        {
            self.report.first_visible = popup;
        }
        let frame = match pixels.present(app.app(), app.window_id()) {
            Ok(frame) => frame,
            Err(error) => {
                self.report.record(
                    false,
                    "replay-error",
                    format_smolstr!("{stage}: 持久增量重放失败 {error}"),
                );
                return;
            }
        };
        let current = match &frame {
            PixelFrame::Idle => return,
            PixelFrame::Full(image) => {
                self.report.record(
                    self.report.sampled_frames == 0,
                    "full-replay",
                    format_smolstr!(
                        "{stage}: 出现整帧重放（damage_full={}）",
                        record.damage_full
                    ),
                );
                image
            }
            PixelFrame::Incremental {
                pixels: image,
                damage,
            } => {
                let outside = changed_pixels(&self.previous, image, damage, 1);
                self.report.record(
                    outside == 0,
                    "changed-outside-damage",
                    format_smolstr!(
                        "{stage}: {outside} 个变化像素落在 damage 之外（damage={damage:?}）"
                    ),
                );
                image
            }
        };
        if record.rendered && record.damage_full {
            self.report.damage_full_frames += 1;
            self.report.record(
                false,
                "damage-full",
                format_smolstr!("{stage}: damage_full=true（damage={:?}）", record.damage),
            );
        }
        if compare_background {
            let mut excluded: Vec<DeviceRect> = Vec::with_capacity(2);
            if let Some(rect) = popup {
                excluded.push(pixel_check_exclusion(app, rect));
            }
            if let Some(rect) = picker
                .trigger_node()
                .and_then(|node| app.probe().bounds(node))
            {
                excluded.push(pixel_check_exclusion(app, rect));
            }
            let diff = compare_pixels(&self.before, current, &excluded);
            self.report.record(
                diff.span <= PIXEL_CHECK_TOLERANCE,
                "background-residue",
                format_smolstr!(
                    "{stage}: popup 外背景残留 span={} at {:?} expected={:?} actual={:?}（排除 {} 个矩形）",
                    diff.span,
                    diff.at,
                    pixel(&self.before, diff.at.0, diff.at.1),
                    pixel(current, diff.at.0, diff.at.1),
                    excluded.len()
                ),
            );
        }
        self.report.sampled_frames += 1;
        self.previous = current.clone();
    }

    fn assert_restored(&mut self, stage: &'static str) {
        let diff = compare_pixels(&self.before, &self.previous, &[]);
        self.report.record(
            diff.span <= PIXEL_CHECK_TOLERANCE,
            "restored",
            format_smolstr!(
                "{stage}: 必须回到打开前像素，残留 span={} at {:?} expected={:?} actual={:?}",
                diff.span,
                diff.at,
                pixel(&self.before, diff.at.0, diff.at.1),
                pixel(&self.previous, diff.at.0, diff.at.1)
            ),
        );
    }

    fn assert_old_popup_restored(
        &mut self,
        app: &TestApp,
        picker: &ColorPicker,
        stage: &'static str,
        old_popup: LogicalRect,
        old_trigger: Option<LogicalRect>,
    ) {
        let mut excluded: Vec<DeviceRect> = Vec::with_capacity(3);
        if let Some(rect) = pixel_check_popup(app, picker) {
            excluded.push(pixel_check_exclusion(app, rect));
        }
        if let Some(rect) = picker
            .trigger_node()
            .and_then(|node| app.probe().bounds(node))
        {
            excluded.push(pixel_check_exclusion(app, rect));
        }
        if let Some(rect) = old_trigger {
            excluded.push(pixel_check_exclusion(app, rect));
        }
        let region = pixel_check_exclusion(app, old_popup);
        let diff = compare_region(
            &self.before,
            &self.previous,
            region,
            &excluded,
            PIXEL_CHECK_TOLERANCE,
        );
        let fixture_pixels = u64::from(self.before.width) * u64::from(self.before.height);
        let required = fixture_pixels * PIXEL_CHECK_MIN_COMPARED_PERMILLE / 1_000;
        self.report.record(
            diff.mismatched == 0,
            "old-region",
            format_smolstr!(
                "{stage}: 旧浮层区域 {region:?} 必须逐像素回到打开前基线（比较 {} 像素，不一致 {}，最大 span={} at {:?}）",
                diff.compared,
                diff.mismatched,
                diff.span,
                diff.at
            ),
        );
        self.report.record(
            u64::from(diff.compared) >= required,
            "old-region-coverage",
            format_smolstr!(
                "{stage}: 旧浮层区域必须真的被比较到（比较 {} 像素，下限 {required} = fixture {fixture_pixels} 设备像素的 {PIXEL_CHECK_MIN_COMPARED_PERMILLE}‰；防止排除区膨胀吞掉检查）",
                diff.compared
            ),
        );
        let detected = residue_detector_self_check(&self.before, region, &excluded);
        self.report.record(
            detected,
            "residue-detector",
            format_smolstr!(
                "{stage}: 在旧浮层区域注入 ≥2 个分散旧色板像素后残影检测必须 FAIL（负向自检）"
            ),
        );
    }
}

fn pixel_check_popup(app: &TestApp, picker: &ColorPicker) -> Option<LogicalRect> {
    let node = picker.panel_node()?;
    app.probe().bounds(node)
}

fn pixel_check_exclusion(app: &TestApp, popup: LogicalRect) -> DeviceRect {
    let window = app.app().window(app.window_id()).expect("窗口");
    window
        .device_pixel_ratio()
        .to_covering_device_rect(popup.outset(px(PIXEL_CHECK_POPUP_MARGIN)))
}

struct RegionDiff {
    compared: u32,
    mismatched: u32,
    span: u8,
    at: (u32, u32),
    first: Option<(u32, u32)>,
    last: Option<(u32, u32)>,
}

fn device_bounds(rect: &DeviceRect) -> (u32, u32, u32, u32) {
    (
        rect.x0.value().max(0) as u32,
        rect.y0.value().max(0) as u32,
        rect.x1.value().max(0) as u32,
        rect.y1.value().max(0) as u32,
    )
}

fn compare_region(
    reference: &SnapshotImage,
    actual: &SnapshotImage,
    region: DeviceRect,
    excluded: &[DeviceRect],
    tolerance: u8,
) -> RegionDiff {
    assert_eq!(
        (reference.width, reference.height),
        (actual.width, actual.height),
        "区域像素对比需要相同的 readback 尺寸"
    );
    let mut diff = RegionDiff {
        compared: 0,
        mismatched: 0,
        span: 0,
        at: (0, 0),
        first: None,
        last: None,
    };
    let (x0, y0, x1, y1) = device_bounds(&region);
    let excluded: Vec<(u32, u32, u32, u32)> = excluded.iter().map(device_bounds).collect();
    for y in y0.min(actual.height)..y1.min(actual.height) {
        for x in x0.min(actual.width)..x1.min(actual.width) {
            if excluded
                .iter()
                .any(|(x0, y0, x1, y1)| x >= *x0 && x < *x1 && y >= *y0 && y < *y1)
            {
                continue;
            }
            diff.first.get_or_insert((x, y));
            diff.last = Some((x, y));
            diff.compared += 1;
            let span = channel_span(pixel(reference, x, y), pixel(actual, x, y));
            if span > tolerance {
                diff.mismatched += 1;
                if span > diff.span {
                    diff.span = span;
                    diff.at = (x, y);
                }
            }
        }
    }
    diff
}

fn residue_detector_self_check(
    baseline: &SnapshotImage,
    region: DeviceRect,
    excluded: &[DeviceRect],
) -> bool {
    let untouched = compare_region(baseline, baseline, region, excluded, 0);
    let (Some(first), Some(last)) = (untouched.first, untouched.last) else {
        return false;
    };
    if first == last {
        return false;
    }
    let mut corrupted = baseline.clone();
    for (x, y) in [first, last] {
        let offset = ((y as usize) * (baseline.width as usize) + x as usize) * 4;
        corrupted.rgba8[offset..offset + 4].copy_from_slice(&[0xFF, 0x00, 0xFF, 0xFF]);
    }
    let detected = compare_region(
        baseline,
        &corrupted,
        region,
        excluded,
        PIXEL_CHECK_TOLERANCE,
    );
    detected.mismatched >= 2
}

fn pixel_check_tick(
    app: &mut TestApp,
    picker: &ColorPicker,
    pixels: &mut PersistentPixels,
    run: &mut PixelCheckRun,
    stage: &'static str,
    compare_background: bool,
) -> bool {
    app.advance(Duration::from_millis(PIXEL_CHECK_FRAME_STEP_MS));
    let record = app.drain().expect("帧").clone();
    let idle = !record.rendered;
    run.observe(app, picker, pixels, &record, stage, compare_background);
    let _ = picker.sync(app.app_mut());
    idle
}

fn pixel_check_settle(
    app: &mut TestApp,
    picker: &ColorPicker,
    pixels: &mut PersistentPixels,
    run: &mut PixelCheckRun,
    stage: &'static str,
    compare_background: bool,
) -> bool {
    let mut idle_streak = 0usize;
    for _ in 0..PIXEL_CHECK_SETTLE_LIMIT {
        idle_streak = if pixel_check_tick(app, picker, pixels, run, stage, compare_background) {
            idle_streak + 1
        } else {
            0
        };
        if idle_streak >= 2 {
            return true;
        }
    }
    false
}

fn pixel_check_close(
    app: &mut TestApp,
    picker: &ColorPicker,
    pixels: &mut PersistentPixels,
    run: &mut PixelCheckRun,
    stage: &'static str,
) {
    let mut closed = false;
    for _ in 0..PIXEL_CHECK_SETTLE_LIMIT {
        pixel_check_tick(app, picker, pixels, run, stage, false);
        if picker.popup_node().is_none() {
            closed = true;
            break;
        }
    }
    run.report.record(
        closed,
        "close-frames",
        format_smolstr!("{stage}: 关闭必须在 {PIXEL_CHECK_SETTLE_LIMIT} 帧内完成"),
    );
    for _ in 0..4 {
        pixel_check_tick(app, picker, pixels, run, stage, false);
    }
}

fn pixel_check_scene(app: &mut TestApp) -> ColorPicker {
    let window = app.window_id();
    let theme = ThemeHandle::new(app.app_mut(), Appearance::Dark);
    let root = app.app().window(window).expect("窗口").tree().root();
    let size = app.app().window(window).expect("窗口").size();
    let backdrop = Color::from_rgba8([0x10, 0x14, 0x1A, 0xFF]);
    {
        let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
        NodeBuilder::boxed()
            .id("gallery.pixel-check:backdrop")
            .layout_override(LogicalRect::from_origin_size(
                LogicalPoint::new(px(-2.0), px(-2.0)),
                LogicalSize::new(size.width + px(4.0), size.height + px(4.0)),
            ))
            .paint(PaintSpec {
                background: backdrop,
                ..PaintSpec::default()
            })
            .build(tree, root)
            .expect("背景");
    }
    let cards = [
        [0x32, 0x3A, 0x46, 0xFF],
        [0x3C, 0x2E, 0x44, 0xFF],
        [0x2C, 0x3E, 0x38, 0xFF],
        [0x44, 0x3A, 0x28, 0xFF],
        [0x28, 0x34, 0x48, 0xFF],
        [0x3E, 0x2A, 0x2A, 0xFF],
    ];
    for (index, color) in cards.into_iter().enumerate() {
        let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
        NodeBuilder::boxed()
            .id(format_smolstr!("gallery.pixel-check:card{index}"))
            .layout_override(LogicalRect::new(
                px(40.0 + (index % 3) as f32 * 240.0),
                px(60.0 + (index / 3) as f32 * 90.0),
                px(220.0),
                px(70.0),
            ))
            .paint(PaintSpec {
                background: Color::from_rgba8(color),
                ..PaintSpec::default()
            })
            .build(tree, root)
            .expect("内容卡");
    }
    let picker = ColorPicker::new(
        app.app_mut(),
        theme,
        "gallery.pixel-check",
        Color::WHITE,
        UnifiedSize::Medium,
    );
    picker.build(app.app_mut(), window, root).expect("触发器");
    pixel_check_move_trigger(app, &picker, PIXEL_CHECK_TRIGGER_A);
    picker
}

const PIXEL_CHECK_TRIGGER_A: LogicalRect =
    LogicalRect::new(px(620.0), px(470.0), px(30.0), px(30.0));
const PIXEL_CHECK_TRIGGER_B: LogicalRect =
    LogicalRect::new(px(120.0), px(360.0), px(30.0), px(30.0));
const PIXEL_CHECK_TRIGGER_C: LogicalRect =
    LogicalRect::new(px(620.0), px(300.0), px(30.0), px(30.0));

#[must_use]
fn pixel_check_rect_near(a: LogicalRect, b: LogicalRect) -> bool {
    (a.origin.x - b.origin.x).abs().value() < 0.5
        && (a.origin.y - b.origin.y).abs().value() < 0.5
        && (a.size.width - b.size.width).abs().value() < 0.5
        && (a.size.height - b.size.height).abs().value() < 0.5
}

fn pixel_check_move_trigger(app: &mut TestApp, picker: &ColorPicker, target: LogicalRect) {
    let window = app.window_id();
    let trigger = picker.trigger_node().expect("触发器已挂载");
    app.app_mut()
        .window_mut(window)
        .expect("窗口")
        .tree_mut()
        .set_layout_override(trigger, Some(ui_core::layout::LayoutOverride::new(target)))
        .expect("移动触发器");
}

fn pixel_check_first(
    app: &TestApp,
    window: WindowId,
    pixels: &mut PersistentPixels,
    record: &FrameRecord,
) -> Option<SnapshotImage> {
    match pixels.present_record(app.app(), window, record) {
        Ok(PixelFrame::Full(image)) => Some(image),
        Ok(other) => {
            println!("gallery: pixel-check FAIL 首帧必须是整帧：{other:?}");
            None
        }
        Err(error) => {
            println!("gallery: pixel-check FAIL 首帧重放失败：{error}");
            None
        }
    }
}

fn run_pixel_check() -> bool {
    println!(
        "gallery: pixel-check semantics: headless persistent-incremental pixel gate \
         (exit 0 = PASS, exit 3 = FAIL); residue judgement = old-popup region pixel-by-pixel \
         against the pre-open baseline with a coverage floor (compared >= {PIXEL_CHECK_MIN_COMPARED_PERMILLE} permille of the \
         fixture device pixels) and a negative self-check that corrupts >= 2 scattered detected pixels; \
         real-window pixel readback/screenshot is P3 (--debug-stack only dumps popup_stack rects); \
         sampled frame count follows the engine popup animation constants (open 120 ms / close 90 ms), \
         every rendered frame is sampled; sampled-animation-floor requires >= 4 open-animation samples \
         and >= 3 samples for each of the two close animations (close#1/close#2) so a shorter animation \
         cannot silently shrink coverage"
    );
    println!(
        "gallery: pixel-check scene: ColorPicker runs on PopupPanel/engine popup \
         (panel_node/popup_node/popup_state + PopupState); the gate drives the live popup stack, \
         --debug-stack dumps the same stack for windows"
    );
    let _guard = ui_core::test_support::snapshot::gpu_guard();
    let mut app = TestApp::new();
    let window = app.window_id();
    let picker = pixel_check_scene(&mut app);
    let mut pixels = PersistentPixels::new(app.app(), window).expect("持久像素目标");
    let first_record = app.drain().expect("首帧").clone();
    let Some(before) = pixel_check_first(&app, window, &mut pixels, &first_record) else {
        return false;
    };
    let _ = picker.sync(app.app_mut());
    let rgba = before.rgba8.as_chunks::<4>().0;
    let translucent = rgba.iter().filter(|pixel| pixel[3] < 255).count();
    let min_alpha = rgba.iter().map(|pixel| pixel[3]).min().unwrap_or(255);
    let mut run = PixelCheckRun {
        report: PixelCheckReport::default(),
        before: before.clone(),
        previous: before,
    };
    run.report.record(
        translucent == 0,
        "baseline-opaque",
        format_smolstr!(
            "打开前基线必须完全不透明（半透明像素 {translucent} 个，min_alpha={min_alpha}）：否则增量重绘会在覆盖边缘累积 alpha"
        ),
    );

    pixel_check_move_trigger(&mut app, &picker, PIXEL_CHECK_TRIGGER_B);
    let moved_closed = pixel_check_settle(
        &mut app,
        &picker,
        &mut pixels,
        &mut run,
        "move-closed",
        false,
    );
    run.report.record(
        moved_closed,
        "move-closed-settle",
        SmolStr::new_static("move-closed: 关闭态移动触发器后必须稳定"),
    );
    pixel_check_move_trigger(&mut app, &picker, PIXEL_CHECK_TRIGGER_A);
    let returned = pixel_check_settle(
        &mut app,
        &picker,
        &mut pixels,
        &mut run,
        "move-closed-return",
        false,
    );
    run.report.record(
        returned,
        "move-closed-return-settle",
        SmolStr::new_static("move-closed-return: 触发器回到原位后必须稳定"),
    );
    run.assert_restored("move-closed");

    let opened = match picker.open(app.app_mut(), window) {
        Ok(opened) => opened,
        Err(error) => {
            println!("gallery: pixel-check FAIL open: {error}");
            return false;
        }
    };
    run.report
        .record(opened, "open", SmolStr::new_static("open: 面板必须打开"));
    let sampled_before_open = run.report.sampled_frames;
    let settled = pixel_check_settle(&mut app, &picker, &mut pixels, &mut run, "open", true);
    run.report.open_animation_frames = run.report.sampled_frames - sampled_before_open;
    run.report.record(
        settled,
        "open-settle",
        SmolStr::new_static("open: 面板必须在限帧内稳定"),
    );
    run.report.stable = pixel_check_popup(&app, &picker);
    match (run.report.first_visible, run.report.stable) {
        (Some(first), Some(stable)) => run.report.record(
            pixel_check_rect_near(first, stable),
            "first-visible-rect",
            format_smolstr!("open: 首个可见帧 rect={first:?} != 稳定帧 rect={stable:?}"),
        ),
        (first, stable) => run.report.record(
            false,
            "first-visible-missing",
            format_smolstr!("open: 首个可见帧/稳定帧 rect 缺失 first={first:?} stable={stable:?}"),
        ),
    }

    run.report.record(
        !app.app().animations_running(window),
        "idle-no-animation",
        SmolStr::new_static("idle: 采样窗口内不得有动画源（否则 idle 断言变成动画检查）"),
    );
    for _ in 0..PIXEL_CHECK_IDLE_FRAMES {
        app.advance(Duration::from_millis(PIXEL_CHECK_IDLE_STEP_MS));
        let idle = pixel_check_tick(&mut app, &picker, &mut pixels, &mut run, "idle", true);
        run.report.record(
            idle,
            "idle-frames",
            SmolStr::new_static("idle: 打开后稳定窗口内不得继续出帧"),
        );
    }

    let old_popup = run.report.stable;
    let old_trigger = picker
        .trigger_node()
        .and_then(|node| app.probe().bounds(node));
    pixel_check_move_trigger(&mut app, &picker, PIXEL_CHECK_TRIGGER_C);
    let moved_open = pixel_check_settle(&mut app, &picker, &mut pixels, &mut run, "move", false);
    run.report.record(
        moved_open,
        "move-settle",
        SmolStr::new_static("move: 锚点移动后必须稳定"),
    );
    run.report.moved = pixel_check_popup(&app, &picker);
    if let Some(old) = old_popup {
        run.assert_old_popup_restored(&app, &picker, "move", old, old_trigger);
    }
    if let (Some(stable), Some(moved)) = (run.report.stable, run.report.moved) {
        run.report.record(
            !pixel_check_rect_near(stable, moved),
            "move-panel",
            format_smolstr!("move: 面板必须离开旧位置（旧={stable:?} 新={moved:?}）"),
        );
    }

    let closed = picker.close(app.app_mut());
    run.report
        .record(closed, "close", SmolStr::new_static("close: 面板必须关闭"));
    let sampled_before_close = run.report.sampled_frames;
    pixel_check_close(&mut app, &picker, &mut pixels, &mut run, "close");
    run.report.close_animation_frames[0] = run.report.sampled_frames - sampled_before_close;
    pixel_check_move_trigger(&mut app, &picker, PIXEL_CHECK_TRIGGER_A);
    let restored = pixel_check_settle(
        &mut app,
        &picker,
        &mut pixels,
        &mut run,
        "close-restore",
        false,
    );
    run.report.record(
        restored,
        "close-restore-settle",
        SmolStr::new_static("close-restore: 关闭并把触发器放回原位后必须稳定"),
    );
    run.assert_restored("close");

    let reopened = match picker.open(app.app_mut(), window) {
        Ok(reopened) => reopened,
        Err(error) => {
            println!("gallery: pixel-check FAIL reopen: {error}");
            return false;
        }
    };
    run.report.record(
        reopened,
        "reopen",
        SmolStr::new_static("reopen: 面板必须重开"),
    );
    let reopened_settled =
        pixel_check_settle(&mut app, &picker, &mut pixels, &mut run, "reopen", true);
    run.report.record(
        reopened_settled,
        "reopen-settle",
        SmolStr::new_static("reopen: 重开后必须稳定"),
    );
    if let (Some(stable), Some(after)) = (run.report.stable, pixel_check_popup(&app, &picker)) {
        run.report.record(
            pixel_check_rect_near(stable, after),
            "reopen-rect",
            format_smolstr!("reopen: 重开面板 rect={after:?} 必须与首次稳定 rect={stable:?} 一致"),
        );
    }
    let closed_again = picker.close(app.app_mut());
    run.report.record(
        closed_again,
        "reopen-close",
        SmolStr::new_static("reopen-close: 面板必须关闭"),
    );
    let sampled_before_reopen_close = run.report.sampled_frames;
    pixel_check_close(&mut app, &picker, &mut pixels, &mut run, "reopen-close");
    run.report.close_animation_frames[1] = run.report.sampled_frames - sampled_before_reopen_close;
    run.assert_restored("reopen-close");

    let open_samples = run.report.open_animation_frames;
    let close_first_samples = run.report.close_animation_frames[0];
    let close_second_samples = run.report.close_animation_frames[1];
    run.report.record(
        open_samples >= 4,
        "sampled-animation-floor",
        format_smolstr!(
            "open 动画采样帧 {open_samples} < 4（引擎 POPUP_OPEN_DURATION=120 ms；动画常量变化不得让采样静默缩水）"
        ),
    );
    run.report.record(
        close_first_samples >= 3,
        "sampled-animation-floor",
        format_smolstr!(
            "close#1 动画采样帧 {close_first_samples} < 3（引擎 POPUP_CLOSE_DURATION=90 ms；单次关闭动画消失不得被累计采样掩盖）"
        ),
    );
    run.report.record(
        close_second_samples >= 3,
        "sampled-animation-floor",
        format_smolstr!(
            "close#2 动画采样帧 {close_second_samples} < 3（引擎 POPUP_CLOSE_DURATION=90 ms；单次关闭动画消失不得被累计采样掩盖）"
        ),
    );

    let report = run.report;
    let passed = report.failures == 0;
    println!(
        "gallery: pixel-check checks={} sampled_frames={} open_animation_frames={} close_animation_frames={}+{}={} damage_full_frames={} first_visible={:?} stable={:?} moved={:?}",
        report.checks,
        report.sampled_frames,
        report.open_animation_frames,
        close_first_samples,
        close_second_samples,
        close_first_samples + close_second_samples,
        report.damage_full_frames,
        report.first_visible,
        report.stable,
        report.moved
    );
    if passed {
        println!(
            "gallery: pixel-check PASS ({} checks; residue = old-region baseline scan with coverage floor + 2-point injected-fault self-check; exit 0)",
            report.checks
        );
    } else {
        for (key, count) in &report.seen {
            println!("gallery: pixel-check failures {key}={count}");
        }
        println!(
            "gallery: pixel-check FAIL ({} failure(s) in {} checks; exit 3)",
            report.failures, report.checks
        );
    }
    passed
}
const REAL_PIXEL_WINDOW_W: f32 = 960.0;
const REAL_PIXEL_WINDOW_H: f32 = 720.0;
const REAL_PIXEL_MARKER_BORDER: f32 = 6.0;
const REAL_PIXEL_TRIGGER_X: f32 = 540.0;
const REAL_PIXEL_TRIGGER_Y: f32 = 200.0;
const REAL_PIXEL_TRIGGER_SIZE: f32 = 40.0;
const REAL_PIXEL_TICK_MS: u64 = 120;
const REAL_PIXEL_WARMUP_MS: u64 = 1_500;
const REAL_PIXEL_BASELINE_GAP_MS: u64 = 400;
const REAL_PIXEL_CAPTURE_DELAY_MS: u64 = 180;
const REAL_PIXEL_MIN_STAGE_MS: u64 = 220;
const REAL_PIXEL_MENU_STEP_MS: u64 = 180;
const REAL_PIXEL_STAGE_TIMEOUT_MS: u64 = 20_000;
const REAL_PIXEL_WATCHDOG_SECS: u64 = 180;
const REAL_PIXEL_RESIDUE_TOLERANCE: u8 = 2;
const REAL_PIXEL_OPEN_TOLERANCE: u8 = 8;
const REAL_PIXEL_OPEN_PERMILLE: u64 = 50;
const REAL_PIXEL_DIALOG_PANEL_PERMILLE: u64 = 200;
const REAL_PIXEL_SCRIM_PERMILLE: u64 = 500;
const REAL_PIXEL_VISIBLE_DELTA: u8 = 40;
const REAL_PIXEL_COVERAGE_PERMILLE: u64 = 990;
const REAL_PIXEL_SCRIM_ALPHA_TOLERANCE: f32 = 0.035;
const REAL_PIXEL_SCRIM_PANEL_MARGIN: u32 = 96;
const REAL_PIXEL_SCRIM_EDGE_MARGIN: u32 = 16;
const REAL_PIXEL_CORNER_INSET: u32 = 4;
const REAL_PIXEL_SCRIM_OUTLIER_PERMILLE: u64 = 10;
const REAL_PIXEL_TRIGGER_MARGIN: u32 = 10;
const REAL_PIXEL_INTERACT_TRIGGER: (f32, f32, f32, f32) = (60.0, 80.0, 150.0, 36.0);
const REAL_PIXEL_INTERACT_REPLAY: (f32, f32, f32, f32) = (700.0, 132.0, 150.0, 36.0);
const REAL_PIXEL_INTERACT_STATUS: (f32, f32, f32, f32) = (700.0, 184.0, 150.0, 24.0);
const REAL_PIXEL_SCROLL_VIEWPORT: (f32, f32, f32, f32) = (700.0, 236.0, 150.0, 110.0);
const REAL_PIXEL_SCROLL_CONTENT_H: f32 = 400.0;
const REAL_PIXEL_SCROLL_ANCHOR: (f32, f32, f32, f32) = (10.0, 80.0, 130.0, 30.0);
const REAL_PIXEL_SCROLL_DELTA: f32 = 80.0;
const REAL_PIXEL_INTERACT_STEP_MS: u64 = 140;
const REAL_PIXEL_RESIZE_W: f32 = 1120.0;
const REAL_PIXEL_RESIZE_H: f32 = 840.0;
const REAL_PIXEL_INTERACT_PERMILLE: u64 = 30;
const REAL_PIXEL_CAPTURE_RETRIES: u8 = 6;
const REAL_PIXEL_CAPTURE_RETRY_MS: u64 = 150;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RealPixelStage {
    #[default]
    Warmup,
    Baseline,
    PickerOpening,
    PickerClosing,
    MenuOpening,
    MenuClosing,
    DialogOpening,
    DialogClosing,
    InteractPointerOpen,
    InteractHover,
    InteractSafeTriangle,
    InteractSibling,
    InteractEscape,
    InteractOutsideReplay,
    InteractKeyboard,
    InteractResizeBaseline,
    InteractScrollOpen,
    InteractScrollRestore,
    InteractSelfChecks,
    Done,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RealPixelWait {
    #[default]
    Idle,
    Capture,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RealPixelTone {
    #[default]
    Full,
    Scrim,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RealPixelRegion {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl RealPixelRegion {
    const fn new(x0: u32, y0: u32, x1: u32, y1: u32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    fn width(self) -> u32 {
        self.x1.saturating_sub(self.x0)
    }

    fn height(self) -> u32 {
        self.y1.saturating_sub(self.y0)
    }

    fn area(self) -> u64 {
        u64::from(self.width()) * u64::from(self.height())
    }

    fn contains(self, x: u32, y: u32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }

    fn clamp(self, width: u32, height: u32) -> Option<Self> {
        let region = Self::new(
            self.x0.min(width),
            self.y0.min(height),
            self.x1.min(width),
            self.y1.min(height),
        );
        (region.width() > 0 && region.height() > 0).then_some(region)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RealPixelDiff {
    compared: u64,
    differing: u64,
    max_delta: u8,
}

#[derive(Clone, Copy, Debug, Default)]
struct RealPixelMarker {
    origin: (u32, u32),
    size: (u32, u32),
    thickness: u32,
}

struct RealPixelBaseline {
    content: RgbaImage,
    origin: (u32, u32),
    size: (u32, u32),
    thickness: u32,
    scale: f32,
    logical: LogicalSize,
}

#[derive(Default)]
struct RealPixelReport {
    stage: RealPixelStage,
    wait: RealPixelWait,
    stage_issued: Option<Instant>,
    stage_frame: u32,
    wait_since: Option<Instant>,
    started: Option<Instant>,
    completed: bool,
    checks: u32,
    failures: Vec<SmolStr>,
    lines: Vec<SmolStr>,
    warmup: Option<RealPixelBaseline>,
    picker_rect: Option<LogicalRect>,
    menu_rects: Vec<LogicalRect>,
    menu_levels: u8,
    menu_level2: Option<Menu>,
    menu_level3: Option<Menu>,
    dialog_panel: Option<LogicalRect>,
    scrim_rect: Option<LogicalRect>,
    trigger_exclude: Option<RealPixelRegion>,
    platform: &'static str,
    capture_command: SmolStr,
    evidence: PathBuf,
    screen: (u32, u32),
    window_logical: LogicalSize,
    dpi: f32,
    last_frame: u32,
    source_revision: SmolStr,
    source_dirty: SmolStr,
    source_diff_sha: SmolStr,
    source_identity: SmolStr,
    build_profile: SmolStr,
    scrim: [f32; 4],
    report_write_failed: bool,
    interact_step: u8,
    interact_at: Option<Instant>,
    interact_root_node: Option<NodeId>,
    interact_root_rect: Option<LogicalRect>,
    interact_level2_node: Option<NodeId>,
    interact_level2_rect: Option<LogicalRect>,
    interact_sibling_node: Option<NodeId>,
    interact_sibling_rect: Option<LogicalRect>,
    interact_activation: Option<SmolStr>,
    interact_replay_on: bool,
    interact_status_on: bool,
    interact_highlighted: Option<usize>,
    interact_open_content: Option<RgbaImage>,
    resize_root_rect: Option<LogicalRect>,
    scroll_rect: Option<LogicalRect>,
    scroll_delta: f32,
    marker_retries: u32,
}

impl RealPixelReport {
    fn record(&mut self, ok: bool, label: &'static str, detail: SmolStr) {
        self.checks = self.checks.saturating_add(1);
        let line = if ok {
            format_smolstr!("PASS {label}: {detail}")
        } else {
            format_smolstr!("FAIL {label}: {detail}")
        };
        println!("gallery: real-pixel-check {line}");
        self.lines.push(line);
        if !ok {
            self.failures.push(format_smolstr!("{label}: {detail}"));
        }
    }

    fn note(&mut self, detail: SmolStr) {
        println!("gallery: real-pixel-check note {detail}");
        self.lines.push(format_smolstr!("note {detail}"));
    }
}

#[derive(Clone)]
struct RealPixelAcceptance {
    window: WindowId,
    picker: ColorPicker,
    chain_menu: Menu,
    dialog: Dialog,
    backdrop: NodeId,
    marker: NodeId,
    trigger: NodeId,
    size: Rc<Cell<LogicalSize>>,
    report: Rc<RefCell<RealPixelReport>>,
    baseline: Rc<RefCell<Option<RealPixelBaseline>>>,
    theme: ThemeHandle,
    interact_trigger: NodeId,
    replay_node: NodeId,
    status_node: NodeId,
    scroll_viewport: NodeId,
    scroll_anchor: NodeId,
    scroll_panel: PopupPanel,
    replay_on: Signal<bool>,
    status_on: Signal<bool>,
    activated: Rc<RefCell<Option<SmolStr>>>,
}

fn real_pixel_evidence_dir() -> PathBuf {
    if let Some(run_dir) = std::env::var_os("KYTRIX_REAL_PIXEL_EVIDENCE") {
        return PathBuf::from(run_dir);
    }
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .map_or_else(|| PathBuf::from("target"), Path::to_path_buf)
                .join("target")
        },
        PathBuf::from,
    );
    target.join("real-pixels").join(std::env::consts::OS)
}

fn real_pixel_capture_command() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "powershell -NoProfile -NonInteractive -Command <System.Drawing CopyFromScreen of the virtual screen -> PNG>"
    }
    #[cfg(target_os = "macos")]
    {
        "screencapture -x <tmp.png> (full screen, no cursor, no sound)"
    }
    #[cfg(target_os = "linux")]
    {
        "grim <tmp.png> (Wayland output layout, no cursor)"
    }
}

fn real_pixel_run_process(program: &str, args: &[&str]) -> Result<(), SmolStr> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format_smolstr!("{program} 启动失败：{error}"))?;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format_smolstr!("{program} 截图超时（45 s）"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format_smolstr!("{program} 等待失败：{error}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format_smolstr!("{program} 收集输出失败：{error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format_smolstr!(
        "{program} 退出码 {:?} stderr={} stdout={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&output.stdout).trim()
    ))
}

#[cfg(target_os = "windows")]
fn real_pixel_capture_file(path: &Path) -> Result<(), SmolStr> {
    let script = format!(
        "Add-Type -AssemblyName System.Drawing; Add-Type -AssemblyName System.Windows.Forms; \
         $vs=[System.Windows.Forms.SystemInformation]::VirtualScreen; \
         $bmp=New-Object System.Drawing.Bitmap $vs.Width,$vs.Height; \
         $g=[System.Drawing.Graphics]::FromImage($bmp); \
         $g.CopyFromScreen($vs.X,$vs.Y,0,0,$bmp.Size); \
         $bmp.Save('{}',[System.Drawing.Imaging.ImageFormat]::Png); \
         $g.Dispose(); $bmp.Dispose()",
        path.display()
    );
    real_pixel_run_process(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
    )
}

#[cfg(target_os = "macos")]
fn real_pixel_capture_file(path: &Path) -> Result<(), SmolStr> {
    let target = path.to_string_lossy();
    real_pixel_run_process("screencapture", &["-x", &target])
}

#[cfg(target_os = "linux")]
fn real_pixel_capture_file(path: &Path) -> Result<(), SmolStr> {
    let target = path.to_string_lossy();
    real_pixel_run_process("grim", &[&target])
}

fn real_pixel_stage_slug(stage: RealPixelStage) -> &'static str {
    match stage {
        RealPixelStage::Warmup => "00-warmup",
        RealPixelStage::Baseline => "00-baseline",
        RealPixelStage::PickerOpening => "01-picker-open",
        RealPixelStage::PickerClosing => "02-picker-closed",
        RealPixelStage::MenuOpening => "03-menu-open",
        RealPixelStage::MenuClosing => "04-menu-closed",
        RealPixelStage::DialogOpening => "05-dialog-open",
        RealPixelStage::DialogClosing => "06-dialog-closed",
        RealPixelStage::InteractPointerOpen => "07-interact-open",
        RealPixelStage::InteractHover => "08-interact-hover",
        RealPixelStage::InteractSafeTriangle => "09-interact-safe-triangle",
        RealPixelStage::InteractSibling => "10-interact-sibling",
        RealPixelStage::InteractEscape => "11-interact-esc-closed",
        RealPixelStage::InteractOutsideReplay => "12-interact-outside-replay",
        RealPixelStage::InteractKeyboard => "13-interact-keyboard",
        RealPixelStage::InteractResizeBaseline => "14-resize-reanchor",
        RealPixelStage::InteractScrollOpen => "15-scroll-reanchored",
        RealPixelStage::InteractScrollRestore => "16-scroll-restored",
        RealPixelStage::InteractSelfChecks => "17-self-checks",
        RealPixelStage::Done => "done",
    }
}

fn real_pixel_allows_resize(stage: RealPixelStage) -> bool {
    matches!(stage, RealPixelStage::InteractResizeBaseline)
}

fn real_pixel_interaction_stage(stage: RealPixelStage) -> bool {
    matches!(
        stage,
        RealPixelStage::InteractPointerOpen
            | RealPixelStage::InteractHover
            | RealPixelStage::InteractSafeTriangle
            | RealPixelStage::InteractSibling
            | RealPixelStage::InteractEscape
            | RealPixelStage::InteractOutsideReplay
            | RealPixelStage::InteractKeyboard
            | RealPixelStage::InteractResizeBaseline
            | RealPixelStage::InteractScrollOpen
            | RealPixelStage::InteractScrollRestore
            | RealPixelStage::InteractSelfChecks
    )
}

fn real_pixel_row_point(rect: LogicalRect, rows: usize, row: usize) -> LogicalPoint {
    let rows = rows.max(1) as f32;
    LogicalPoint::new(
        px(rect.origin.x.value() + rect.size.width.value() * 0.5),
        px(rect.origin.y.value() + rect.size.height.value() * (row as f32 + 0.5) / rows),
    )
}

fn real_pixel_rect_center(rect: LogicalRect) -> LogicalPoint {
    LogicalPoint::new(
        px(rect.origin.x.value() + rect.size.width.value() * 0.5),
        px(rect.origin.y.value() + rect.size.height.value() * 0.5),
    )
}

fn real_pixel_visible_change(
    baseline: &RealPixelBaseline,
    shot: &RgbaImage,
    rect: LogicalRect,
    minimum_permille: u64,
) -> bool {
    let Some(region) = real_pixel_map_rect(baseline, rect, 2) else {
        return false;
    };
    let diff = real_pixel_compare(
        &baseline.content,
        shot,
        region,
        REAL_PIXEL_OPEN_TOLERANCE,
        None,
    );
    let permille = diff
        .compared
        .saturating_mul(1000)
        .checked_div(region.area())
        .unwrap_or(0);
    permille >= minimum_permille && diff.max_delta >= REAL_PIXEL_VISIBLE_DELTA
}

fn real_pixel_node_region(
    baseline: &RealPixelBaseline,
    app: &App,
    window: WindowId,
    node: NodeId,
    margin: u32,
) -> Option<RealPixelRegion> {
    let rect = app.window(window)?.node_screen_rect(node)?;
    real_pixel_map_rect(baseline, rect, margin)
}

fn real_pixel_region_change(
    baseline: &RgbaImage,
    shot: &RgbaImage,
    region: RealPixelRegion,
    tolerance: u8,
) -> (RealPixelDiff, u64) {
    let diff = real_pixel_compare(baseline, shot, region, tolerance, None);
    let permille = diff
        .differing
        .saturating_mul(1000)
        .checked_div(region.area())
        .unwrap_or(0);
    (diff, permille)
}

fn real_pixel_marker_pixel(pixel: [u8; 4], tone: RealPixelTone) -> bool {
    let [red, green, blue, _alpha] = pixel;
    match tone {
        RealPixelTone::Full => red >= 200 && blue >= 200 && green <= 60 && red.abs_diff(blue) <= 40,
        RealPixelTone::Scrim => {
            red >= 80
                && blue >= 80
                && red <= 235
                && blue <= 235
                && green <= 40
                && red.abs_diff(blue) <= 32
        }
    }
}

fn real_pixel_detect_marker(
    shot: &RgbaImage,
    expected_w: u32,
    expected_h: u32,
    tone: RealPixelTone,
) -> Option<RealPixelMarker> {
    let (width, height) = shot.dimensions();
    if width == 0 || height == 0 || expected_w == 0 || expected_h == 0 {
        return None;
    }
    let min_run = ((expected_w as f32 * 0.8) as u32).max(32) as usize;
    let max_run = ((expected_w as f32 * 1.2) as u32 + 8) as usize;
    let raw = shot.as_raw();
    let stride = width as usize * 4;
    let mut rows: Vec<(u32, u32, u32)> = Vec::with_capacity(64);
    for y in 0..height as usize {
        let row = &raw[y * stride..(y + 1) * stride];
        let mut best_start = 0usize;
        let mut best_len = 0usize;
        let mut run_start = 0usize;
        let mut run_len = 0usize;
        for (x, pixel) in row.as_chunks::<4>().0.iter().enumerate() {
            if real_pixel_marker_pixel([pixel[0], pixel[1], pixel[2], pixel[3]], tone) {
                if run_len == 0 {
                    run_start = x;
                }
                run_len += 1;
                if run_len > best_len {
                    best_len = run_len;
                    best_start = run_start;
                }
            } else {
                run_len = 0;
            }
        }
        if best_len >= min_run && best_len <= max_run {
            rows.push((y as u32, best_start as u32, (best_start + best_len) as u32));
        }
    }
    if rows.len() < 2 {
        return None;
    }
    let mut groups: Vec<Vec<(u32, u32, u32)>> = Vec::with_capacity(4);
    for row in rows {
        let consecutive = groups
            .last()
            .and_then(|group| group.last())
            .is_some_and(|last| row.0 <= last.0 + 1);
        if consecutive {
            if let Some(group) = groups.last_mut() {
                group.push(row);
            }
        } else {
            groups.push(vec![row]);
        }
    }
    let height_tolerance = (expected_h as f32 * 0.02 + 4.0).max(8.0);
    let width_tolerance = expected_w as f32 * 0.02 + 4.0;
    let group_bounds = |group: &[(u32, u32, u32)]| -> Option<(u32, u32, u32, u32, u32)> {
        let first = group.first()?;
        let last = group.last()?;
        let widest = group.iter().max_by_key(|row| row.2.saturating_sub(row.1))?;
        Some((first.0, last.0, widest.1, widest.2, last.0 + 1 - first.0))
    };
    let mut best: Option<(f32, RealPixelMarker)> = None;
    for top_index in 0..groups.len() {
        for bottom_index in (top_index + 1)..groups.len() {
            let (
                Some((top_y0, _top_y1, top_x0, top_x1, top_thickness)),
                Some((_bottom_y0, bottom_y1, bottom_x0, bottom_x1, _bottom_thickness)),
            ) = (
                group_bounds(&groups[top_index]),
                group_bounds(&groups[bottom_index]),
            )
            else {
                continue;
            };
            let pixel_height = bottom_y1 + 1 - top_y0;
            if (pixel_height as f32 - expected_h as f32).abs() > height_tolerance {
                continue;
            }
            let pixel_width = top_x1 - top_x0;
            if (pixel_width as f32 - expected_w as f32).abs() > width_tolerance {
                continue;
            }
            if top_x0.abs_diff(bottom_x0) > 3 || top_x1.abs_diff(bottom_x1) > 3 {
                continue;
            }
            if top_thickness == 0 || top_thickness > 64 {
                continue;
            }
            let score = top_x0.abs_diff(bottom_x0) as f32
                + top_x1.abs_diff(bottom_x1) as f32
                + (pixel_width as f32 - expected_w as f32).abs()
                + (pixel_height as f32 - expected_h as f32).abs();
            if best
                .as_ref()
                .is_none_or(|(best_score, _)| score < *best_score)
            {
                best = Some((
                    score,
                    RealPixelMarker {
                        origin: (top_x0, top_y0),
                        size: (pixel_width, pixel_height),
                        thickness: top_thickness,
                    },
                ));
            }
        }
    }
    best.map(|(_, marker)| marker)
}

fn real_pixel_crop(shot: &RgbaImage, marker: RealPixelMarker) -> Option<RgbaImage> {
    let (width, height) = shot.dimensions();
    let right = marker.origin.0.checked_add(marker.size.0)?;
    let bottom = marker.origin.1.checked_add(marker.size.1)?;
    if marker.size.0 == 0 || marker.size.1 == 0 || right > width || bottom > height {
        return None;
    }
    Some(
        image::imageops::crop_imm(
            shot,
            marker.origin.0,
            marker.origin.1,
            marker.size.0,
            marker.size.1,
        )
        .to_image(),
    )
}

fn real_pixel_compare(
    base: &RgbaImage,
    shot: &RgbaImage,
    region: RealPixelRegion,
    tolerance: u8,
    exclusion: Option<RealPixelRegion>,
) -> RealPixelDiff {
    let mut diff = RealPixelDiff::default();
    let (width, height) = base.dimensions();
    if shot.dimensions() != (width, height) {
        return diff;
    }
    let Some(region) = region.clamp(width, height) else {
        return diff;
    };
    let base_raw = base.as_raw();
    let shot_raw = shot.as_raw();
    let stride = width as usize * 4;
    for y in region.y0..region.y1 {
        let row = y as usize * stride;
        for x in region.x0..region.x1 {
            if exclusion.is_some_and(|excluded| excluded.contains(x, y)) {
                continue;
            }
            let index = row + x as usize * 4;
            let left = &base_raw[index..index + 4];
            let right = &shot_raw[index..index + 4];
            let delta = left[0]
                .abs_diff(right[0])
                .max(left[1].abs_diff(right[1]))
                .max(left[2].abs_diff(right[2]))
                .max(left[3].abs_diff(right[3]));
            diff.compared = diff.compared.saturating_add(1);
            if delta > tolerance {
                diff.differing = diff.differing.saturating_add(1);
            }
            diff.max_delta = diff.max_delta.max(delta);
        }
    }
    diff
}

fn real_pixel_shifted(image: &RgbaImage, dx: i32, dy: i32) -> RgbaImage {
    let (width, height) = image.dimensions();
    let mut shifted = RgbaImage::new(width, height);
    if width == 0 || height == 0 {
        return shifted;
    }
    for y in 0..height {
        for x in 0..width {
            let source_x = (i64::from(x) - i64::from(dx)).clamp(0, i64::from(width - 1)) as u32;
            let source_y = (i64::from(y) - i64::from(dy)).clamp(0, i64::from(height - 1)) as u32;
            shifted.put_pixel(x, y, *image.get_pixel(source_x, source_y));
        }
    }
    shifted
}

fn real_pixel_recolored(image: &RgbaImage, region: RealPixelRegion) -> RgbaImage {
    let mut recolored = image.clone();
    let Some(region) = region.clamp(image.width(), image.height()) else {
        return recolored;
    };
    let color = Rgba([0xFF, 0x00, 0xFF, 0xFF]);
    for y in region.y0..region.y1 {
        for x in region.x0..region.x1 {
            recolored.put_pixel(x, y, color);
        }
    }
    recolored
}

fn real_pixel_content_region(baseline: &RealPixelBaseline) -> RealPixelRegion {
    RealPixelRegion::new(0, 0, baseline.size.0, baseline.size.1)
}

fn real_pixel_content_inner_region(baseline: &RealPixelBaseline) -> RealPixelRegion {
    let region = real_pixel_content_region(baseline);
    let inset =
        (REAL_PIXEL_MARKER_BORDER * baseline.scale).round() as u32 + REAL_PIXEL_CORNER_INSET;
    RealPixelRegion::new(
        region.x0.saturating_add(inset),
        region.y0.saturating_add(inset),
        region.x1.saturating_sub(inset),
        region.y1.saturating_sub(inset),
    )
}

fn real_pixel_map_rect(
    baseline: &RealPixelBaseline,
    rect: LogicalRect,
    inflate: u32,
) -> Option<RealPixelRegion> {
    let dpr = DevicePixelRatio::new(baseline.scale)?;
    let device = dpr.to_covering_device_rect(rect);
    let content_w = i32::try_from(baseline.size.0).ok()?;
    let content_h = i32::try_from(baseline.size.1).ok()?;
    let margin = i32::try_from(inflate).ok()?;
    let x0 = (i32::from(device.x0) - margin).clamp(0, content_w);
    let y0 = (i32::from(device.y0) - margin).clamp(0, content_h);
    let x1 = (i32::from(device.x1) + margin).clamp(0, content_w);
    let y1 = (i32::from(device.y1) + margin).clamp(0, content_h);
    (x1 > x0 && y1 > y0).then(|| RealPixelRegion::new(x0 as u32, y0 as u32, x1 as u32, y1 as u32))
}

fn real_pixel_contained(baseline: &RealPixelBaseline, rect: LogicalRect) -> bool {
    let Some(dpr) = DevicePixelRatio::new(baseline.scale) else {
        return false;
    };
    let device = dpr.to_covering_device_rect(rect);
    i32::from(device.x0) >= -1
        && i32::from(device.y0) >= -1
        && i32::from(device.x1) <= baseline.size.0 as i32 + 1
        && i32::from(device.y1) <= baseline.size.1 as i32 + 1
}

fn real_pixel_rect_detail(rect: LogicalRect) -> SmolStr {
    format_smolstr!(
        "({:.1},{:.1} {:.1}x{:.1})",
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height
    )
}

fn real_pixel_visibility_check(
    report: &mut RealPixelReport,
    label: &'static str,
    baseline: &RealPixelBaseline,
    shot: &RgbaImage,
    rect: LogicalRect,
    minimum_permille: u64,
) {
    let Some(region) = real_pixel_map_rect(baseline, rect, 2) else {
        report.record(
            false,
            label,
            SmolStr::new_static("popup rect 无法映射到窗口内容区"),
        );
        return;
    };
    let diff = real_pixel_compare(
        &baseline.content,
        shot,
        region,
        REAL_PIXEL_OPEN_TOLERANCE,
        None,
    );
    let permille = diff
        .compared
        .saturating_mul(1000)
        .checked_div(region.area())
        .unwrap_or(0);
    report.record(
        permille >= minimum_permille && diff.max_delta >= REAL_PIXEL_VISIBLE_DELTA,
        label,
        format_smolstr!(
            "rect={} region={region:?} compared={} changed={} ({permille}‰) max_delta={}",
            real_pixel_rect_detail(rect),
            diff.compared,
            diff.differing,
            diff.max_delta
        ),
    );
}

fn real_pixel_residue_check(
    report: &mut RealPixelReport,
    label: &'static str,
    baseline: &RealPixelBaseline,
    shot: &RgbaImage,
    rect: LogicalRect,
) {
    let Some(region) = real_pixel_map_rect(baseline, rect, 2) else {
        report.record(
            false,
            label,
            SmolStr::new_static("popup rect 无法映射到窗口内容区"),
        );
        return;
    };
    let diff = real_pixel_compare(
        &baseline.content,
        shot,
        region,
        REAL_PIXEL_RESIDUE_TOLERANCE,
        None,
    );
    let permille = diff
        .compared
        .saturating_mul(1000)
        .checked_div(region.area())
        .unwrap_or(0);
    report.record(
        diff.differing == 0 && permille >= REAL_PIXEL_COVERAGE_PERMILLE,
        label,
        format_smolstr!(
            "rect={} region={region:?} compared={} coverage={permille}‰ differing={} max_delta={} tolerance={REAL_PIXEL_RESIDUE_TOLERANCE}",
            real_pixel_rect_detail(rect),
            diff.compared,
            diff.differing,
            diff.max_delta
        ),
    );
}

fn real_pixel_content_residue_check(
    report: &mut RealPixelReport,
    label: &'static str,
    baseline: &RealPixelBaseline,
    shot: &RgbaImage,
) {
    let region = real_pixel_content_inner_region(baseline);
    let exclusion = report.trigger_exclude;
    let expected = region
        .area()
        .saturating_sub(exclusion.map_or(0, RealPixelRegion::area));
    let diff = real_pixel_compare(
        &baseline.content,
        shot,
        region,
        REAL_PIXEL_RESIDUE_TOLERANCE,
        exclusion,
    );
    let permille = diff
        .compared
        .saturating_mul(1000)
        .checked_div(expected)
        .unwrap_or(0);
    report.record(
        diff.differing == 0 && permille >= REAL_PIXEL_COVERAGE_PERMILLE,
        label,
        format_smolstr!(
            "content={region:?} excluded={exclusion:?} compared={} coverage={permille}‰ differing={} max_delta={} tolerance={REAL_PIXEL_RESIDUE_TOLERANCE}",
            diff.compared,
            diff.differing,
            diff.max_delta
        ),
    );
}

fn real_pixel_srgb_to_linear(value: u8) -> f32 {
    let channel = f32::from(value) / 255.0;
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn real_pixel_linear_to_srgb(value: f32) -> u8 {
    let channel = value.clamp(0.0, 1.0);
    let encoded = if channel <= 0.003_130_8 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

fn real_pixel_scrim_predicted(base: &RgbaImage, scrim: [f32; 4]) -> RgbaImage {
    let mut predicted = RgbaImage::new(base.width(), base.height());
    let alpha = scrim[3].clamp(0.0, 1.0);
    for (x, y, pixel) in base.enumerate_pixels() {
        let mut out = [0u8; 4];
        for channel in 0..3 {
            let linear = scrim[channel] + (1.0 - alpha) * real_pixel_srgb_to_linear(pixel[channel]);
            out[channel] = real_pixel_linear_to_srgb(linear);
        }
        out[3] = 255;
        predicted.put_pixel(x, y, Rgba(out));
    }
    predicted
}

fn real_pixel_derive_scrim_alpha(
    baseline: &RealPixelBaseline,
    shot: &RgbaImage,
    scrim: [f32; 4],
) -> Option<(f32, u64)> {
    let (width, height) = shot.dimensions();
    if width != baseline.content.width() || height != baseline.content.height() {
        return None;
    }
    let thickness = baseline.thickness.max(2);
    let y = ((thickness - 1) / 2).max(1);
    let x0 = width / 5;
    let x1 = width - x0;
    let mut samples: Vec<f32> = Vec::with_capacity((x1 - x0) as usize * 3);
    for x in x0..x1 {
        let base = baseline.content.get_pixel(x, y).0;
        let observed = shot.get_pixel(x, y).0;
        for channel in 0..3 {
            let base_linear = real_pixel_srgb_to_linear(base[channel]);
            if base_linear < 0.2 {
                continue;
            }
            let observed_linear = real_pixel_srgb_to_linear(observed[channel]);
            let derived = 1.0 - (observed_linear - scrim[channel]) / base_linear;
            if derived.is_finite() && (0.0..=1.0).contains(&derived) {
                samples.push(derived);
            }
        }
    }
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(f32::total_cmp);
    Some((samples[samples.len() / 2], samples.len() as u64))
}

fn real_pixel_compare_multi(
    base: &RgbaImage,
    shot: &RgbaImage,
    region: RealPixelRegion,
    tolerance: u8,
    exclusions: &[RealPixelRegion],
) -> RealPixelDiff {
    let mut diff = RealPixelDiff::default();
    let (width, height) = base.dimensions();
    if shot.dimensions() != (width, height) {
        return diff;
    }
    let Some(region) = region.clamp(width, height) else {
        return diff;
    };
    let base_raw = base.as_raw();
    let shot_raw = shot.as_raw();
    let stride = width as usize * 4;
    for y in region.y0..region.y1 {
        let row = y as usize * stride;
        for x in region.x0..region.x1 {
            if exclusions.iter().any(|excluded| excluded.contains(x, y)) {
                continue;
            }
            let index = row + x as usize * 4;
            let left = &base_raw[index..index + 4];
            let right = &shot_raw[index..index + 4];
            let delta = left[0]
                .abs_diff(right[0])
                .max(left[1].abs_diff(right[1]))
                .max(left[2].abs_diff(right[2]))
                .max(left[3].abs_diff(right[3]));
            diff.compared = diff.compared.saturating_add(1);
            if delta > tolerance {
                diff.differing = diff.differing.saturating_add(1);
            }
            diff.max_delta = diff.max_delta.max(delta);
        }
    }
    diff
}

fn real_pixel_scrim_analysis(
    predicted: &RgbaImage,
    shot: &RgbaImage,
    region: RealPixelRegion,
    exclusions: &[RealPixelRegion],
) -> (RealPixelDiff, RealPixelDiff) {
    let loose = real_pixel_compare_multi(predicted, shot, region, 4, exclusions);
    let strict = real_pixel_compare_multi(predicted, shot, region, 12, exclusions);
    (loose, strict)
}

fn real_pixel_source_identity_from_file() -> Option<(SmolStr, SmolStr, SmolStr)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/source-id.txt");
    let text = std::fs::read_to_string(path).ok()?;
    let mut revision = None;
    let mut status = None;
    let mut diff_sha = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("revision=") {
            revision = Some(SmolStr::new(value.trim()));
        } else if let Some(value) = line.strip_prefix("status=") {
            status = Some(SmolStr::new(value.trim()));
        } else if let Some(value) = line.strip_prefix("diff_sha=") {
            diff_sha = Some(SmolStr::new(value.trim()));
        }
    }
    Some((revision?, status?, diff_sha?))
}

fn real_pixel_source_revision() -> (SmolStr, SmolStr, SmolStr, SmolStr) {
    use std::io::Write as _;
    if let Some((revision, status, diff_sha)) = real_pixel_source_identity_from_file() {
        return (
            revision,
            status,
            diff_sha,
            SmolStr::new_static("source-id-file"),
        );
    }
    let manifest = env!("CARGO_MANIFEST_DIR");
    let git = |args: &[&str]| {
        Command::new("git")
            .args(["-C", manifest])
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
    };
    let revision = git(&["rev-parse", "--short", "HEAD"]).map_or_else(
        || SmolStr::new_static("unknown"),
        |output| SmolStr::new(String::from_utf8_lossy(&output.stdout).trim()),
    );
    let status = git(&["status", "--porcelain"]).map(|output| {
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.lines().filter(|line| !line.trim().is_empty()).count()
    });
    let dirty = match status {
        None => SmolStr::new_static("unknown"),
        Some(0) => SmolStr::new_static("clean"),
        Some(count) => format_smolstr!("dirty({count} files)"),
    };
    let diff_sha = git(&["diff", "HEAD"])
        .and_then(|output| {
            let mut child = Command::new("git")
                .args(["-C", manifest, "hash-object", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .ok()?;
            child
                .stdin
                .take()
                .and_then(|mut stdin| stdin.write_all(&output.stdout).ok())?;
            child.wait_with_output().ok()
        })
        .filter(|output| output.status.success())
        .map_or_else(
            || SmolStr::new_static("unknown"),
            |output| SmolStr::new(String::from_utf8_lossy(&output.stdout).trim()),
        );
    (revision, dirty, diff_sha, SmolStr::new_static("git"))
}

fn real_pixel_theme_scrim(theme: &ThemeHandle) -> [f32; 4] {
    let color = theme.theme_for(Appearance::Dark).role(Role::Scrim);
    ColorResolver.resolve(color).components
}

fn real_pixel_stack_entries(
    app: &App,
    window: WindowId,
) -> Vec<ui_core::popup::PopupSnapshotEntry> {
    app.popup_stack(window)
        .map_or_else(Vec::new, |stack| stack.entries)
}

fn real_pixel_stack_ready(app: &App, window: WindowId, expected: usize) -> bool {
    if app.animations_running(window) {
        return false;
    }
    let entries = real_pixel_stack_entries(app, window);
    entries.len() == expected
        && entries
            .iter()
            .all(|entry| entry.state == ui_core::popup::PopupState::Open)
}

fn real_pixel_stack_empty(app: &App, window: WindowId) -> bool {
    if app.animations_running(window) {
        return false;
    }
    app.popup_stack(window)
        .is_some_and(|stack| stack.entries.is_empty())
}

fn real_pixel_menu_result(result: &Result<Option<Menu>, ui_core::popup::PopupError>) -> SmolStr {
    match result {
        Ok(Some(_)) => SmolStr::new_static("Some(menu)"),
        Ok(None) => SmolStr::new_static("None"),
        Err(error) => format_smolstr!("Err({error})"),
    }
}

impl RealPixelAcceptance {
    fn build(app: &mut App) -> Self {
        println!(
            "gallery: real-pixel-check semantics: true-window screenshot gate (exit 0/3); a fixed \
             {REAL_PIXEL_WINDOW_W}×{REAL_PIXEL_WINDOW_H} always-on-top window draws a \
             {REAL_PIXEL_MARKER_BORDER} px #FF00FF ring at the content edge; the full-screen screenshot is \
             decoded, the magenta box gives the content origin/physical size/scale, engine popup rects are \
             mapped through the window scale factor, and the sequence baseline → ColorPicker open/close → \
             3-level MenuChain open/close → Dialog+scrim open/close → real-pointer/keyboard interaction \
             (click-open, hover submenu, safe-triangle diagonal move, sibling replacement, per-level Esc, \
             outside click + Replay to a background control, Tab/arrow/Enter keyboard path, scroll \
             re-anchor, window resize re-anchor) must change each popup region on open and restore it \
             pixel-for-pixel on close (coverage floor; trigger ring excluded); interactive steps are driven \
             through the platform input path (InputEvent injection) and negative self-checks for the \
             interaction detectors (static baseline as open, open frame as residue, wrong level count, \
             static replay region) must turn red"
        );
        println!(
            "gallery: real-pixel-check platform={} capture={}",
            std::env::consts::OS,
            real_pixel_capture_command()
        );
        println!(
            "gallery: real-pixel-check platform notes: windows = CopyFromScreen of the virtual screen \
             (multi-monitor offsets and per-window DPR are resolved from the marker box); macos = \
             screencapture -x needs Screen Recording for the launching session, a plain ssh session only \
             captures the wallpaper (launch through Terminal, see tools/real-pixel-check-macos.sh); linux = \
             grim captures the whole wl_output layout and a tiled nested-sway window is resized to the \
             output, so the content size is the compositor size, not the requested 960×720"
        );
        println!(
            "gallery: real-pixel-check note: the Dialog scrim covers the border marker, so the dialog-open \
             step first retries the marker with the scrim-dimmed tone and otherwise falls back to the baseline \
             origin (printed as a note); the marker is re-detected and compared after close"
        );
        println!(
            "gallery: real-pixel-check retry note: the companion gate scripts (tools/real-pixel-check-*) run \
             --pixel-check/--real-pixel-check/--idle-check/--theme-check/--debug-stack; under a busy or \
             concurrent desktop --debug-stack can exceed the 300 s gate timeout, the scripts retry it up to 3 \
             attempts and a single timeout is retried without changing the gate result (final result = first \
             successful attempt)"
        );
        let evidence = real_pixel_evidence_dir();
        for name in [
            "report.txt",
            "00-warmup.png",
            "00-baseline.png",
            "01-picker-open.png",
            "02-picker-closed.png",
            "03-menu-open.png",
            "04-menu-closed.png",
            "05-dialog-open.png",
            "06-dialog-closed.png",
            "07-interact-open.png",
            "08-interact-hover.png",
            "09-interact-safe-triangle.png",
            "10-interact-sibling.png",
            "11-interact-esc-closed.png",
            "12-interact-outside-replay.png",
            "13-interact-keyboard.png",
            "14-resize-reanchor.png",
            "15-scroll-reanchored.png",
            "16-scroll-restored.png",
            "17-self-checks.png",
            "failed-marker-00-warmup.png",
        ] {
            let _ = std::fs::remove_file(evidence.join(name));
        }
        if let Err(error) = std::fs::create_dir_all(&evidence) {
            println!(
                "gallery: real-pixel-check FAIL evidence dir {}: {error}",
                evidence.display()
            );
            std::process::exit(3);
        }
        println!("gallery: real-pixel-check evidence={}", evidence.display());
        let theme = ThemeHandle::new(app, Appearance::Dark);
        let scrim = real_pixel_theme_scrim(&theme);
        let (source_revision, source_dirty, source_diff_sha, source_identity) =
            real_pixel_source_revision();
        println!(
            "gallery: real-pixel-check metadata revision={source_revision} status={source_dirty} diff_sha={source_diff_sha} identity={source_identity} profile={} scrim_alpha={:.3}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            scrim[3]
        );
        let window = app.create_window(WindowOptions {
            title: SmolStr::new_static("Kytrix · gallery real pixel acceptance"),
            size: LogicalSize::new(px(REAL_PIXEL_WINDOW_W), px(REAL_PIXEL_WINDOW_H)),
            position: Some(LogicalPoint::new(px(120.0), px(90.0))),
            always_on_top: true,
            ..WindowOptions::default()
        });
        let size = app.window(window).map_or(
            LogicalSize::new(px(REAL_PIXEL_WINDOW_W), px(REAL_PIXEL_WINDOW_H)),
            |target| target.size(),
        );
        let root = tree(app, window).root();
        tree(app, window)
            .set_style(root, StyleBuilder::new().flex_col().size_full().build())
            .expect("real-pixel root style");
        let full = LogicalRect::from_origin_size(LogicalPoint::ZERO, size);
        let backdrop = NodeBuilder::boxed()
            .id("gallery.real-pixel:backdrop")
            .layout_override(full)
            .paint(PaintSpec {
                background: Color::from_rgba8([0x10, 0x14, 0x1A, 0xFF]),
                ..PaintSpec::default()
            })
            .build(tree(app, window), root)
            .expect("real-pixel backdrop");
        let cards: [(LogicalRect, [u8; 4]); 4] = [
            (
                LogicalRect::new(px(60.0), px(430.0), px(240.0), px(120.0)),
                [0x2A, 0x32, 0x3E, 0xFF],
            ),
            (
                LogicalRect::new(px(330.0), px(430.0), px(240.0), px(120.0)),
                [0x36, 0x2E, 0x3E, 0xFF],
            ),
            (
                LogicalRect::new(px(600.0), px(430.0), px(240.0), px(120.0)),
                [0x2E, 0x3E, 0x38, 0xFF],
            ),
            (
                LogicalRect::new(px(60.0), px(580.0), px(300.0), px(100.0)),
                [0x3A, 0x36, 0x2A, 0xFF],
            ),
        ];
        for (index, (rect, rgb)) in cards.into_iter().enumerate() {
            NodeBuilder::boxed()
                .id(format_smolstr!("gallery.real-pixel:card{index}"))
                .layout_override(rect)
                .paint(PaintSpec {
                    background: Color::from_rgba8(rgb),
                    ..PaintSpec::default()
                })
                .build(tree(app, window), root)
                .expect("real-pixel card");
        }
        let picker = ColorPicker::new(
            app,
            theme.clone(),
            "gallery.real-pixel-picker",
            Color::from_rgba8([0x4C, 0x9A, 0xFF, 0xFF]),
            UnifiedSize::Medium,
        );
        let _ = picker.build(app, window, root).expect("real-pixel picker");
        let trigger = picker.trigger_node().expect("real-pixel picker trigger");
        tree(app, window)
            .set_layout_override(
                trigger,
                Some(ui_core::layout::LayoutOverride::new(LogicalRect::new(
                    px(REAL_PIXEL_TRIGGER_X),
                    px(REAL_PIXEL_TRIGGER_Y),
                    px(REAL_PIXEL_TRIGGER_SIZE),
                    px(REAL_PIXEL_TRIGGER_SIZE),
                ))),
            )
            .expect("real-pixel picker trigger placement");
        let marker = NodeBuilder::boxed()
            .id("gallery.real-pixel:marker")
            .layout_override(full)
            .paint(PaintSpec {
                border: Some(Border {
                    width: px(REAL_PIXEL_MARKER_BORDER),
                    color: Color::from_rgba8([0xFF, 0x00, 0xFF, 0xFF]),
                }),
                ..PaintSpec::default()
            })
            .build(tree(app, window), root)
            .expect("real-pixel marker");
        let replay_on = app.signal(false);
        let status_on = app.signal(false);
        let activated: Rc<RefCell<Option<SmolStr>>> = Rc::new(RefCell::new(None));
        let chain_menu = Menu::new(
            app,
            theme.clone(),
            "gallery.real-pixel-menu",
            [
                MenuEntry::Action(MenuAction::new("first", "第一项").checkbox(true)),
                MenuEntry::Action(MenuAction::new("second", "第二项")),
                MenuEntry::Submenu(MenuSubmenu::new(
                    "二级",
                    [
                        MenuEntry::Action(MenuAction::new("inner", "内部")),
                        MenuEntry::Submenu(MenuSubmenu::new(
                            "三级",
                            [
                                MenuEntry::Action(MenuAction::new("third", "第三项").radio(true)),
                                MenuEntry::Action(MenuAction::new("fourth", "第四项")),
                            ],
                        )),
                    ],
                )),
                MenuEntry::Submenu(MenuSubmenu::new(
                    "二级B",
                    [MenuEntry::Action(MenuAction::new("sibling", "兄弟项"))],
                )),
            ],
            MenuOptions::default(),
        );
        let activated_for_menu = Rc::clone(&activated);
        let status_for_activate = status_on;
        let chain_menu = chain_menu.on_activate(move |app, activation: MenuActivation| {
            *activated_for_menu.borrow_mut() = Some(activation.id);
            app.write(&status_for_activate, true);
        });
        let menu_for_trigger = chain_menu.clone();
        let interact_trigger =
            Button::new(app, theme.clone(), "gallery.real-pixel:interact-trigger")
                .label("菜单触发")
                .on_click(move |ctx| {
                    let window = ctx.window();
                    let node = ctx.node();
                    let app = ctx.app_mut();
                    let _ =
                        menu_for_trigger.open(app, window, ui_core::popup::Anchor::Node { node });
                })
                .build(app, window, root)
                .expect("real-pixel interact trigger");
        tree(app, window)
            .set_layout_override(
                interact_trigger,
                Some(ui_core::layout::LayoutOverride::new(LogicalRect::new(
                    px(REAL_PIXEL_INTERACT_TRIGGER.0),
                    px(REAL_PIXEL_INTERACT_TRIGGER.1),
                    px(REAL_PIXEL_INTERACT_TRIGGER.2),
                    px(REAL_PIXEL_INTERACT_TRIGGER.3),
                ))),
            )
            .expect("real-pixel interact trigger placement");
        let replay_rect = LogicalRect::new(
            px(REAL_PIXEL_INTERACT_REPLAY.0),
            px(REAL_PIXEL_INTERACT_REPLAY.1),
            px(REAL_PIXEL_INTERACT_REPLAY.2),
            px(REAL_PIXEL_INTERACT_REPLAY.3),
        );
        let replay_action = Action::Custom(SmolStr::new_static("gallery.real-pixel:replay"));
        let replay_signal = replay_on;
        let replay_node = NodeBuilder::boxed()
            .id("gallery.real-pixel:replay")
            .layout_override(replay_rect)
            .on_click(replay_action.clone())
            .bind_style(move |cx| StylePatch {
                background: Some(if *cx.read(&replay_signal) {
                    Color::from_rgba8([0x3C, 0x8C, 0x48, 0xFF])
                } else {
                    Color::from_rgba8([0x5C, 0x2A, 0x2A, 0xFF])
                }),
                ..StylePatch::default()
            })
            .build(tree(app, window), root)
            .expect("real-pixel replay target");
        let replay_signal_for_action = replay_on;
        app.on_action(replay_action, move |ctx| {
            let next = !*ctx.read(&replay_signal_for_action);
            ctx.write(&replay_signal_for_action, next);
        });
        let status_rect = LogicalRect::new(
            px(REAL_PIXEL_INTERACT_STATUS.0),
            px(REAL_PIXEL_INTERACT_STATUS.1),
            px(REAL_PIXEL_INTERACT_STATUS.2),
            px(REAL_PIXEL_INTERACT_STATUS.3),
        );
        let status_signal = status_on;
        let status_node = NodeBuilder::boxed()
            .id("gallery.real-pixel:status")
            .layout_override(status_rect)
            .bind_style(move |cx| StylePatch {
                background: Some(if *cx.read(&status_signal) {
                    Color::from_rgba8([0x3C, 0x6C, 0x9C, 0xFF])
                } else {
                    Color::from_rgba8([0x2A, 0x2E, 0x36, 0xFF])
                }),
                ..StylePatch::default()
            })
            .build(tree(app, window), root)
            .expect("real-pixel status box");
        let viewport_style = StyleBuilder::new()
            .absolute()
            .left(px(REAL_PIXEL_SCROLL_VIEWPORT.0))
            .top(px(REAL_PIXEL_SCROLL_VIEWPORT.1))
            .size(
                px(REAL_PIXEL_SCROLL_VIEWPORT.2),
                px(REAL_PIXEL_SCROLL_VIEWPORT.3),
            )
            .overflow_hidden()
            .build();
        let scroll_viewport = NodeBuilder::boxed()
            .id("gallery.real-pixel:scroll")
            .style(viewport_style)
            .build(tree(app, window), root)
            .expect("real-pixel scroll viewport");
        let scroll_content = NodeBuilder::boxed()
            .id("gallery.real-pixel:scroll-content")
            .style(
                StyleBuilder::new()
                    .size(
                        px(REAL_PIXEL_SCROLL_VIEWPORT.2),
                        px(REAL_PIXEL_SCROLL_CONTENT_H),
                    )
                    .build(),
            )
            .build(tree(app, window), scroll_viewport)
            .expect("real-pixel scroll content");
        let scroll_anchor = NodeBuilder::boxed()
            .id("gallery.real-pixel:scroll-anchor")
            .layout_override(LogicalRect::new(
                px(REAL_PIXEL_SCROLL_ANCHOR.0),
                px(REAL_PIXEL_SCROLL_ANCHOR.1),
                px(REAL_PIXEL_SCROLL_ANCHOR.2),
                px(REAL_PIXEL_SCROLL_ANCHOR.3),
            ))
            .paint(PaintSpec {
                background: Color::from_rgba8([0x24, 0x8A, 0x9A, 0xFF]),
                ..PaintSpec::default()
            })
            .build(tree(app, window), scroll_content)
            .expect("real-pixel scroll anchor");
        app.make_scroll(window, scroll_viewport, ScrollConfig::default())
            .expect("real-pixel scroll setup");
        let scroll_panel = PopupPanel::new(
            app,
            theme.clone(),
            "gallery.real-pixel:scroll-panel",
            PopupPanelOptions::new(PopupSurface::menu(), PanelWidth::Exact(px(140.0))),
        );
        let scroll_handle = app
            .scroll_handle(window, scroll_viewport)
            .expect("real-pixel scroll handle");
        let _ = app.set_scroll_content(
            &scroll_handle,
            LogicalSize::new(
                px(REAL_PIXEL_SCROLL_VIEWPORT.2),
                px(REAL_PIXEL_SCROLL_CONTENT_H),
            ),
        );
        let dialog_theme = theme.clone();
        let dialog = DialogBuilder::new("真实窗口像素验收")
            .content(move |app, window, body| {
                let theme = dialog_theme.clone();
                NodeBuilder::text(
                    "Dialog + scrim 的真实窗口像素验收：打开必须可见变化，关闭必须逐像素回到基线。",
                    text_style(12.5, 400, Color::WHITE).with_wrap(TextWrap::Wrap),
                )
                .style(StyleBuilder::new().width_full().build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme.role(cx, Role::FgDefault)),
                    ..StylePatch::default()
                })
                .build(tree(app, window), body)?;
                Ok(())
            })
            .confirm("确定", |_app| {})
            .cancel("取消", |_app| {})
            .build(app, theme.clone(), "gallery.real-pixel-dialog");
        let report = RealPixelReport {
            platform: std::env::consts::OS,
            capture_command: SmolStr::new_static(real_pixel_capture_command()),
            evidence,
            window_logical: size,
            source_revision,
            source_dirty,
            source_diff_sha,
            source_identity,
            build_profile: SmolStr::new_static(if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }),
            scrim,
            ..RealPixelReport::default()
        };
        Self {
            window,
            picker,
            chain_menu,
            dialog,
            backdrop,
            marker,
            trigger,
            size: Rc::new(Cell::new(size)),
            report: Rc::new(RefCell::new(report)),
            baseline: Rc::new(RefCell::new(None)),
            theme,
            interact_trigger,
            replay_node,
            status_node,
            scroll_viewport,
            scroll_anchor,
            scroll_panel,
            replay_on,
            status_on,
            activated,
        }
    }

    fn install(&self, app: &mut App) {
        {
            let mut report = self.report.borrow_mut();
            let now = Instant::now();
            report.started = Some(now);
            report.stage_issued = Some(now);
        }
        self.schedule(app);
        let this = self.clone();
        let _ = app.after(
            self.window,
            Duration::from_secs(REAL_PIXEL_WATCHDOG_SECS),
            move |app| {
                let finished = {
                    let mut report = this.report.borrow_mut();
                    if report.completed {
                        true
                    } else {
                        report.record(
                            false,
                            "watchdog",
                            format_smolstr!(
                                "--real-pixel-check 自动化在 {REAL_PIXEL_WATCHDOG_SECS} s 内未完成"
                            ),
                        );
                        false
                    }
                };
                if !finished {
                    this.finish(app);
                }
            },
        );
    }

    fn schedule(&self, app: &mut App) {
        let this = self.clone();
        let _ = app.after(
            self.window,
            Duration::from_millis(REAL_PIXEL_TICK_MS),
            move |app| {
                if this.tick(app) {
                    this.schedule(app);
                }
            },
        );
    }

    fn tick(&self, app: &mut App) -> bool {
        self.sync_components(app);
        self.sync_layout(app);
        let Some(window_size) = app.window(self.window).map(|target| target.size()) else {
            return false;
        };
        let frame = app
            .window(self.window)
            .map_or(0, |target| target.frame_index().get());
        let now = Instant::now();
        let stage_now = self.report.borrow().stage;
        let stale_logical = {
            let baseline = self.baseline.borrow();
            baseline
                .as_ref()
                .map(|baseline| baseline.logical)
                .filter(|logical| *logical != window_size)
                .filter(|_| !real_pixel_allows_resize(stage_now))
        };
        if let Some(baseline_logical) = stale_logical {
            let mut report = self.report.borrow_mut();
            report.record(
                false,
                "window-stable",
                format_smolstr!(
                    "验收过程中窗口逻辑尺寸变化：{:?} != {:?}",
                    window_size,
                    baseline_logical
                ),
            );
            drop(report);
            self.finish(app);
            return false;
        }
        let mut report = self.report.borrow_mut();
        if report.completed {
            return false;
        }
        if report.started.is_none() {
            report.started = Some(now);
        }
        if report.stage == RealPixelStage::Done {
            drop(report);
            self.finish(app);
            return false;
        }
        if report.wait == RealPixelWait::Capture {
            let elapsed = report
                .wait_since
                .map_or(Duration::ZERO, |since| now.duration_since(since));
            if elapsed >= Duration::from_millis(REAL_PIXEL_CAPTURE_DELAY_MS)
                && frame > report.stage_frame
            {
                report.wait = RealPixelWait::Idle;
                let keep = self.capture(app, &mut report);
                if !keep {
                    drop(report);
                    self.finish(app);
                    return false;
                }
            }
            return true;
        }
        let stage_issued = report.stage_issued.unwrap_or(now);
        if now.duration_since(stage_issued) > Duration::from_millis(REAL_PIXEL_STAGE_TIMEOUT_MS) {
            let stage = report.stage;
            let entries = real_pixel_stack_entries(app, self.window);
            let states: Vec<String> = entries
                .iter()
                .map(|entry| format!("{:?}/z{}/{:?}", entry.tier, entry.z, entry.state))
                .collect();
            let animations = app.animations_running(self.window);
            report.record(
                false,
                "stage-timeout",
                format_smolstr!(
                    "阶段 {stage:?} 在 {REAL_PIXEL_STAGE_TIMEOUT_MS} ms 内未稳定（动画/输入无法 settle）；stack={states:?} animations_running={animations}"
                ),
            );
            drop(report);
            self.finish(app);
            return false;
        }
        if report.stage == RealPixelStage::MenuOpening && !self.advance_menu(app, &mut report, now)
        {
            drop(report);
            self.finish(app);
            return false;
        }
        if real_pixel_interaction_stage(report.stage)
            && !self.advance_interaction(app, &mut report, now)
        {
            drop(report);
            self.finish(app);
            return false;
        }
        if self.stage_ready(app, &report, now, frame) {
            let _ = app.request_repaint(self.window);
            report.wait = RealPixelWait::Capture;
            report.wait_since = Some(now);
            report.stage_frame = frame;
        }
        true
    }

    fn advance_menu(&self, app: &mut App, report: &mut RealPixelReport, now: Instant) -> bool {
        if report.menu_levels >= 3 {
            return true;
        }
        if report.stage_issued.is_some_and(|issued| {
            now.duration_since(issued) < Duration::from_millis(REAL_PIXEL_MENU_STEP_MS)
        }) {
            return true;
        }
        let entries = real_pixel_stack_entries(app, self.window);
        let settled = |expected: usize| {
            entries.len() == expected
                && entries
                    .iter()
                    .all(|entry| entry.state == ui_core::popup::PopupState::Open)
        };
        match report.menu_levels {
            1 if settled(1) => match self.chain_menu.open_submenu(app, 2) {
                Ok(Some(menu)) => {
                    report.menu_level2 = Some(menu);
                    report.menu_levels = 2;
                    report.record(
                        true,
                        "menu-open-level2",
                        SmolStr::new_static("open_submenu(二级) -> Some"),
                    );
                    report.stage_issued = Some(now);
                    let _ = app.request_repaint(self.window);
                    true
                }
                other => {
                    report.record(
                        false,
                        "menu-open-level2",
                        format_smolstr!("open_submenu(二级) -> {}", real_pixel_menu_result(&other)),
                    );
                    false
                }
            },
            2 if settled(2) => {
                let Some(level2) = report.menu_level2.as_ref() else {
                    report.record(
                        false,
                        "menu-open-level3",
                        SmolStr::new_static("二级 Menu 句柄缺失"),
                    );
                    return false;
                };
                match level2.open_submenu(app, 1) {
                    Ok(Some(menu)) => {
                        report.menu_level3 = Some(menu);
                        report.menu_levels = 3;
                        report.record(
                            true,
                            "menu-open-level3",
                            SmolStr::new_static("open_submenu(三级) -> Some"),
                        );
                        report.stage_issued = Some(now);
                        let _ = app.request_repaint(self.window);
                        true
                    }
                    other => {
                        report.record(
                            false,
                            "menu-open-level3",
                            format_smolstr!(
                                "open_submenu(三级) -> {}",
                                real_pixel_menu_result(&other)
                            ),
                        );
                        false
                    }
                }
            }
            _ => true,
        }
    }

    fn dispatch_pointer(&self, app: &mut App, point: LogicalPoint) {
        let _ = app.dispatch_input_tracked_for_test(
            self.window,
            InputEvent::pointer_move(point, Modifiers::empty()),
        );
    }

    fn dispatch_click(&self, app: &mut App, point: LogicalPoint) {
        self.dispatch_pointer(app, point);
        let _ = app.dispatch_input_tracked_for_test(
            self.window,
            InputEvent::pointer_down(point, PointerButton::Primary, Modifiers::empty()),
        );
        let _ = app.dispatch_input_tracked_for_test(
            self.window,
            InputEvent::pointer_up(point, PointerButton::Primary, Modifiers::empty()),
        );
    }

    fn dispatch_key(&self, app: &mut App, code: KeyCode) {
        let _ = app.dispatch_input_tracked_for_test(
            self.window,
            InputEvent::key_down(code, Modifiers::empty()),
        );
        let _ = app.dispatch_input_tracked_for_test(
            self.window,
            InputEvent::key_up(code, Modifiers::empty()),
        );
    }

    fn interact_trigger_rect(&self, app: &App) -> LogicalRect {
        app.window(self.window)
            .and_then(|target| target.node_screen_rect(self.interact_trigger))
            .unwrap_or_else(|| {
                LogicalRect::new(
                    px(REAL_PIXEL_INTERACT_TRIGGER.0),
                    px(REAL_PIXEL_INTERACT_TRIGGER.1),
                    px(REAL_PIXEL_INTERACT_TRIGGER.2),
                    px(REAL_PIXEL_INTERACT_TRIGGER.3),
                )
            })
    }

    fn interact_entries(&self, app: &App) -> Vec<ui_core::popup::PopupSnapshotEntry> {
        real_pixel_stack_entries(app, self.window)
    }

    fn scroll_handle(&self, app: &App) -> Option<ui_core::interaction::ScrollHandle> {
        app.scroll_handle(self.window, self.scroll_viewport)
    }

    fn scroll_offset(&self, app: &App) -> Option<LogicalPoint> {
        let handle = self.scroll_handle(app)?;
        app.scroll_state(&handle).map(|state| state.offset)
    }

    fn advance_interaction(
        &self,
        app: &mut App,
        report: &mut RealPixelReport,
        now: Instant,
    ) -> bool {
        let stage = report.stage;
        if stage == RealPixelStage::InteractSelfChecks {
            return true;
        }
        let gated = report.interact_at.is_some_and(|at| {
            now.duration_since(at) < Duration::from_millis(REAL_PIXEL_INTERACT_STEP_MS)
        });
        if gated {
            return true;
        }
        let trigger_rect = self.interact_trigger_rect(app);
        let entries = self.interact_entries(app);
        let rects: Vec<LogicalRect> = entries.iter().map(|entry| entry.paint_rect).collect();
        match (stage, report.interact_step) {
            (RealPixelStage::InteractPointerOpen, 0) => {
                report.interact_root_node = None;
                report.interact_root_rect = None;
                report.interact_level2_node = None;
                report.interact_level2_rect = None;
                report.interact_sibling_node = None;
                report.interact_sibling_rect = None;
                report.interact_open_content = None;
                report.interact_step = 1;
                report.interact_at = Some(now);
                self.dispatch_click(app, real_pixel_rect_center(trigger_rect));
            }
            (RealPixelStage::InteractHover, 0) => {
                let Some(root) = rects.first().copied() else {
                    return true;
                };
                let point = real_pixel_row_point(root, self.chain_menu.row_count(), 2);
                report.interact_step = 1;
                report.interact_at = Some(now);
                self.dispatch_pointer(app, point);
            }

            (RealPixelStage::InteractSafeTriangle, 0) => {
                if rects.len() < 2 {
                    return true;
                }
                let root = rects[0];
                let level2 = rects[1];
                report.interact_level2_rect = Some(level2);
                let row3 = real_pixel_row_point(root, self.chain_menu.row_count(), 3);
                let triangle = LogicalPoint::new(px(level2.origin.x.value() - 3.0), row3.y);
                report.interact_step = 1;
                report.interact_at = Some(now);
                self.dispatch_pointer(app, triangle);
            }
            (RealPixelStage::InteractSafeTriangle, 1) => {
                let held = rects.len() == 2;
                report.record(
                    held,
                    "safe-triangle-holds",
                    format_smolstr!(
                        "斜向移动进入 safe triangle 后不得关闭二级：stack={} rects={rects:?}",
                        rects.len()
                    ),
                );
                let Some(level2) = report.interact_level2_rect else {
                    return false;
                };
                let point = real_pixel_row_point(level2, 2, 1);
                report.interact_step = 2;
                report.interact_at = Some(now);
                self.dispatch_pointer(app, point);
            }
            (RealPixelStage::InteractSibling, 0) => {
                let Some(root) = rects.first().copied() else {
                    return true;
                };
                let row3 = real_pixel_row_point(root, self.chain_menu.row_count(), 3);
                let point = LogicalPoint::new(px(root.origin.x.value() + 8.0), row3.y);
                report.interact_step = 1;
                report.interact_at = Some(now);
                self.dispatch_pointer(app, point);
            }
            (RealPixelStage::InteractSibling, 1) => {
                let sibling = rects
                    .get(1)
                    .copied()
                    .zip(entries.get(1))
                    .map(|(rect, entry)| (entry.node, rect));
                if let Some((node, rect)) = sibling
                    && report.interact_level2_node != Some(node)
                {
                    report.interact_sibling_node = Some(node);
                    report.interact_sibling_rect = Some(rect);
                    report.interact_step = 2;
                    report.interact_at = Some(now);
                }
            }
            (RealPixelStage::InteractEscape, 0) => {
                report.record(
                    rects.len() == 2,
                    "escape-before",
                    format_smolstr!("逐层 Esc 起点应为 2 层，实际 stack={}", rects.len()),
                );
                report.interact_step = 1;
                report.interact_at = Some(now);
                self.dispatch_key(app, KeyCode::Escape);
            }
            (RealPixelStage::InteractEscape, 1) => {
                let after_first = rects.len();
                report.record(
                    after_first == 1,
                    "escape-level1",
                    format_smolstr!(
                        "第一次 Esc 只关闭最上层（剩根菜单），实际 stack={after_first}"
                    ),
                );
                report.interact_step = 2;
                report.interact_at = Some(now);
                self.dispatch_key(app, KeyCode::Escape);
            }
            (RealPixelStage::InteractEscape, 2) => {
                let after_second = rects.len();
                report.record(
                    after_second == 0,
                    "escape-level2",
                    format_smolstr!("第二次 Esc 关闭根菜单，实际 stack={after_second}"),
                );
                report.interact_step = 3;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractOutsideReplay, 0) => {
                report.interact_step = 1;
                report.interact_at = Some(now);
                self.dispatch_click(app, real_pixel_rect_center(trigger_rect));
            }
            (RealPixelStage::InteractOutsideReplay, 1) => {
                if rects.len() != 1 {
                    return true;
                }
                let replay_rect = LogicalRect::new(
                    px(REAL_PIXEL_INTERACT_REPLAY.0),
                    px(REAL_PIXEL_INTERACT_REPLAY.1),
                    px(REAL_PIXEL_INTERACT_REPLAY.2),
                    px(REAL_PIXEL_INTERACT_REPLAY.3),
                );
                report.interact_step = 2;
                report.interact_at = Some(now);
                self.dispatch_click(app, real_pixel_rect_center(replay_rect));
            }
            (RealPixelStage::InteractOutsideReplay, 2) => {
                let replay_on = app.read_with(&self.replay_on, |value| *value);
                report.record(
                    rects.is_empty() && !self.chain_menu.is_open(app) && replay_on,
                    "outside-replay",
                    format_smolstr!(
                        "外部按下关闭整链并 Replay 到背景控件：stack={} menu_open={} replay_on={replay_on}",
                        rects.len(),
                        self.chain_menu.is_open(app)
                    ),
                );
                report.interact_step = 3;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractKeyboard, 0) => {
                let mut tabs = 0u8;
                let mut focused = app.focused(self.window);
                while focused != Some(self.interact_trigger) && tabs < 8 {
                    self.dispatch_key(app, KeyCode::Tab);
                    tabs = tabs.saturating_add(1);
                    focused = app.focused(self.window);
                }
                report.record(
                    focused == Some(self.interact_trigger),
                    "keyboard-tab-focus",
                    format_smolstr!("Tab×{tabs} 后焦点={focused:?} 期望 Some(interact_trigger)"),
                );
                report.interact_step = 1;
                report.interact_at = Some(now);
                self.dispatch_key(app, KeyCode::Enter);
            }
            (RealPixelStage::InteractKeyboard, 1) => {
                if rects.len() != 1 {
                    return true;
                }
                let before = self.chain_menu.highlighted(app);
                self.dispatch_key(app, KeyCode::ArrowDown);
                let after = self.chain_menu.highlighted(app);
                report.interact_highlighted = after;
                report.record(
                    after.is_some() && after != before,
                    "keyboard-arrow-highlight",
                    format_smolstr!("方向键移动高亮：{before:?} -> {after:?}"),
                );
                report.interact_step = 2;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractKeyboard, 2) => {
                report.interact_step = 3;
                report.interact_at = Some(now);
                self.dispatch_key(app, KeyCode::Enter);
            }
            (RealPixelStage::InteractKeyboard, 3) => {
                let activation = self.activated.borrow().clone();
                report.interact_activation = activation.clone();
                let status_on = app.read_with(&self.status_on, |value| *value);
                report.record(
                    activation.as_deref() == Some("second") && status_on && rects.is_empty(),
                    "keyboard-enter-activate",
                    format_smolstr!(
                        "Enter 激活第二项并关闭菜单：activation={activation:?} status_on={status_on} stack={}",
                        rects.len()
                    ),
                );
                report.interact_step = 4;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractResizeBaseline, 0) => {
                let anchor = ui_core::popup::Anchor::WindowRect(LogicalRect::new(
                    px(900.0),
                    px(600.0),
                    px(1.0),
                    px(1.0),
                ));
                let _ = self.chain_menu.open(app, self.window, anchor);
                report.interact_step = 1;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractResizeBaseline, 1) => {
                if rects.len() != 1 {
                    return true;
                }
                report.resize_root_rect = rects.first().copied();
                let closed = self.chain_menu.close(app);
                report.record(
                    closed,
                    "resize-preopen",
                    format_smolstr!(
                        "缩放前用 WindowRect(900,600) 锚点打开并记录弹层 rect={:?}，关闭={closed}",
                        report.resize_root_rect
                    ),
                );
                report.interact_step = 2;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractResizeBaseline, 2) => {
                let target = LogicalSize::new(px(REAL_PIXEL_RESIZE_W), px(REAL_PIXEL_RESIZE_H));
                if let Some(window) = app.window_mut(self.window) {
                    window.set_size(target);
                }
                let _ = app.request_repaint(self.window);
                report.interact_step = 3;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractResizeBaseline, 3) => {
                let size = app.window(self.window).map(|window| window.size());
                let target_ok = size.is_some_and(|size| {
                    (size.width.value() - REAL_PIXEL_RESIZE_W).abs() < 0.5
                        && (size.height.value() - REAL_PIXEL_RESIZE_H).abs() < 0.5
                });
                if !target_ok {
                    return true;
                }
                let anchor = ui_core::popup::Anchor::WindowRect(LogicalRect::new(
                    px(900.0),
                    px(600.0),
                    px(1.0),
                    px(1.0),
                ));
                let _ = self.chain_menu.open(app, self.window, anchor);
                report.interact_step = 4;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractResizeBaseline, 4) => {
                if rects.len() != 1 {
                    return true;
                }
                let new_rect = rects[0];
                let reanchored = report.resize_root_rect.is_some_and(|old| {
                    new_rect != old
                        && new_rect.origin.x.value() > old.origin.x.value() + 40.0
                        && new_rect.origin.x.value() + new_rect.size.width.value()
                            <= REAL_PIXEL_RESIZE_W + 0.5
                        && new_rect.origin.y.value() + new_rect.size.height.value()
                            <= REAL_PIXEL_RESIZE_H + 0.5
                });
                report.record(
                    reanchored,
                    "resize-reanchor",
                    format_smolstr!(
                        "窗口逻辑尺寸 960x720→1120x840 后同一 WindowRect(900,600) 锚点弹层必须重新定位且在新窗口内：old={:?} new={}",
                        report.resize_root_rect,
                        real_pixel_rect_detail(new_rect)
                    ),
                );
                let closed = self.chain_menu.close(app);
                report.record(
                    closed,
                    "resize-close-call",
                    format_smolstr!("缩放断言后关闭菜单 -> {closed}"),
                );
                report.interact_step = 5;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractResizeBaseline, 5) => {
                let original = self
                    .baseline
                    .borrow()
                    .as_ref()
                    .map(|baseline| baseline.logical);
                if let Some(original) = original
                    && let Some(window) = app.window_mut(self.window)
                {
                    window.set_size(original);
                }
                let _ = app.request_repaint(self.window);
                report.interact_step = 6;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractResizeBaseline, 6) => {
                let restored = self.baseline.borrow().as_ref().is_some_and(|baseline| {
                    app.window(self.window)
                        .is_some_and(|window| window.size() == baseline.logical)
                });
                if !restored {
                    return true;
                }
                report.record(
                    true,
                    "resize-restored",
                    SmolStr::new_static(
                        "窗口逻辑尺寸已恢复；resize 断言不截屏（宿主不把内部分辨率变化同步到原生窗口），像素一致性由后续 scroll/self-check 阶段证明",
                    ),
                );
                let frame = app
                    .window(self.window)
                    .map_or(0, |window| window.frame_index().get());
                self.advance(app, report, RealPixelStage::InteractScrollOpen, frame);
                return true;
            }
            (RealPixelStage::InteractScrollOpen, 0) => {
                let spec = ui_core::popup::PopupSpec::new(
                    ui_core::popup::Anchor::Node {
                        node: self.scroll_anchor,
                    },
                    ui_core::popup::PlacementSpec::default(),
                );
                let opened = self
                    .scroll_panel
                    .open(app, self.window, spec, |app, window, body| {
                        let Some(target) = app.window_mut(window) else {
                            return Err(ui_core::error::EngineError::UnknownWindow { window });
                        };
                        let tree = target.tree_mut();
                        NodeBuilder::boxed()
                            .style(StyleBuilder::new().size(px(120.0), px(80.0)).build())
                            .paint(PaintSpec {
                                background: Color::from_rgba8([0x2C, 0x5C, 0x86, 0xFF]),
                                ..PaintSpec::default()
                            })
                            .build(tree, body)?;
                        Ok(())
                    });
                report.record(
                    opened.is_ok(),
                    "scroll-open-call",
                    format_smolstr!("滚动锚点打开通用 PopupPanel：{opened:?}"),
                );
                report.interact_step = 1;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractScrollOpen, 1) => {
                if rects.len() != 1 {
                    return true;
                }
                report.scroll_rect = Some(rects[0]);
                let Some(handle) = self.scroll_handle(app) else {
                    report.record(
                        false,
                        "scroll-handle",
                        SmolStr::new_static("缺少 scroll handle"),
                    );
                    return false;
                };
                let _ = app.scroll_by(
                    &handle,
                    LogicalPoint::new(px(0.0), px(REAL_PIXEL_SCROLL_DELTA)),
                );
                report.interact_step = 2;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractScrollOpen, 2) => {
                let Some(before) = report.scroll_rect else {
                    return true;
                };
                let Some(after) = rects.first().copied() else {
                    return true;
                };
                let delta = after.origin.y.value() - before.origin.y.value();
                report.scroll_delta = delta;
                let offset = self
                    .scroll_offset(app)
                    .map_or(0.0, |offset| offset.y.value());
                report.record(
                    delta.abs() > 1.0
                        && (delta.abs() - REAL_PIXEL_SCROLL_DELTA).abs() <= 6.0
                        && offset > 1.0,
                    "scroll-reanchor",
                    format_smolstr!(
                        "滚动 {REAL_PIXEL_SCROLL_DELTA} px 后通用弹层跟随锚点：Δy={delta:.1} offset_y={offset:.1} before={} after={}",
                        real_pixel_rect_detail(before),
                        real_pixel_rect_detail(after)
                    ),
                );
                report.interact_step = 3;
                report.interact_at = Some(now);
            }
            (RealPixelStage::InteractScrollRestore, 0) => {
                if let Some(handle) = self.scroll_handle(app) {
                    let _ = app.scroll_to(&handle, LogicalPoint::ZERO);
                }
                let closed = self.scroll_panel.close(app);
                report.record(
                    closed,
                    "scroll-close-call",
                    format_smolstr!("滚动还原并关闭通用弹层 -> {closed}"),
                );
                report.interact_step = 1;
                report.interact_at = Some(now);
            }
            _ => {}
        }
        true
    }

    fn stage_ready(&self, app: &App, report: &RealPixelReport, now: Instant, frame: u32) -> bool {
        let Some(issued) = report.stage_issued else {
            return false;
        };
        let minimum = match report.stage {
            RealPixelStage::Warmup => REAL_PIXEL_WARMUP_MS,
            RealPixelStage::Baseline => REAL_PIXEL_BASELINE_GAP_MS,
            _ => REAL_PIXEL_MIN_STAGE_MS,
        };
        if frame <= report.stage_frame
            || now.duration_since(issued) < Duration::from_millis(minimum)
        {
            return false;
        }
        match report.stage {
            RealPixelStage::Warmup | RealPixelStage::Baseline | RealPixelStage::Done => true,
            RealPixelStage::PickerOpening => real_pixel_stack_ready(app, self.window, 1),
            RealPixelStage::MenuOpening => {
                real_pixel_stack_ready(app, self.window, 3) && report.menu_levels >= 3
            }
            RealPixelStage::DialogOpening => real_pixel_stack_ready(app, self.window, 2),
            RealPixelStage::PickerClosing
            | RealPixelStage::MenuClosing
            | RealPixelStage::DialogClosing => real_pixel_stack_empty(app, self.window),
            RealPixelStage::InteractPointerOpen => {
                report.interact_step >= 1 && real_pixel_stack_ready(app, self.window, 1)
            }
            RealPixelStage::InteractHover => real_pixel_stack_ready(app, self.window, 2),
            RealPixelStage::InteractSafeTriangle => {
                report.interact_step >= 2 && real_pixel_stack_ready(app, self.window, 3)
            }
            RealPixelStage::InteractSibling => {
                report.interact_step >= 2 && real_pixel_stack_ready(app, self.window, 2)
            }
            RealPixelStage::InteractEscape => {
                report.interact_step >= 3 && real_pixel_stack_empty(app, self.window)
            }
            RealPixelStage::InteractOutsideReplay => {
                report.interact_step >= 3
                    && real_pixel_stack_empty(app, self.window)
                    && app.read_with(&self.replay_on, |value| *value)
            }
            RealPixelStage::InteractKeyboard => {
                report.interact_step >= 4
                    && real_pixel_stack_empty(app, self.window)
                    && app.read_with(&self.status_on, |value| *value)
            }
            RealPixelStage::InteractResizeBaseline => false,
            RealPixelStage::InteractScrollOpen => {
                report.interact_step >= 3
                    && real_pixel_stack_ready(app, self.window, 1)
                    && self
                        .scroll_offset(app)
                        .is_some_and(|offset| offset.y.value() > 1.0)
            }
            RealPixelStage::InteractScrollRestore => {
                report.interact_step >= 1
                    && real_pixel_stack_empty(app, self.window)
                    && self
                        .scroll_offset(app)
                        .is_none_or(|offset| offset.y.value().abs() < 0.5)
            }
            RealPixelStage::InteractSelfChecks => true,
        }
    }

    fn sync_components(&self, app: &mut App) {
        let _ = self.picker.sync(app);
        let _ = self.chain_menu.sync(app);
        let _ = self.dialog.sync(app);
        let _ = self.scroll_panel.sync(app);
    }

    fn sync_layout(&self, app: &mut App) {
        let Some(size) = app.window(self.window).map(|target| target.size()) else {
            return;
        };
        if self.size.get() == size {
            return;
        }
        self.size.set(size);
        let Some(target) = app.window_mut(self.window) else {
            return;
        };
        let tree = target.tree_mut();
        let full = LogicalRect::from_origin_size(LogicalPoint::ZERO, size);
        for node in [self.backdrop, self.marker] {
            let _ =
                tree.set_layout_override(node, Some(ui_core::layout::LayoutOverride::new(full)));
        }
        let _ = app.request_repaint(self.window);
    }

    fn screenshot(&self, stage: RealPixelStage) -> Result<RgbaImage, SmolStr> {
        let path = std::env::temp_dir().join(format!(
            "kytrix-real-pixel-{}-{}.png",
            std::process::id(),
            real_pixel_stage_slug(stage)
        ));
        let _ = std::fs::remove_file(&path);
        real_pixel_capture_file(&path)?;
        let decoded = image::open(&path)
            .map_err(|error| format_smolstr!("截图解码失败 {}：{error}", path.display()))
            .map(|decoded| decoded.to_rgba8());
        let _ = std::fs::remove_file(&path);
        decoded
    }

    fn advance(
        &self,
        app: &mut App,
        report: &mut RealPixelReport,
        stage: RealPixelStage,
        frame: u32,
    ) {
        report.stage = stage;
        report.stage_issued = Some(Instant::now());
        report.stage_frame = frame;
        report.wait = RealPixelWait::Idle;
        report.interact_step = 0;
        report.interact_at = None;
        report.marker_retries = 0;
        let _ = app.request_repaint(self.window);
    }

    fn save_evidence(
        &self,
        report: &mut RealPixelReport,
        stage: RealPixelStage,
        content: &RgbaImage,
    ) -> bool {
        let path = report
            .evidence
            .join(format!("{}.png", real_pixel_stage_slug(stage)));
        match content.save(&path) {
            Ok(()) => {
                let line = format_smolstr!("evidence {}", path.display());
                println!("gallery: real-pixel-check {line}");
                report.lines.push(line);
                true
            }
            Err(error) => {
                report.record(
                    false,
                    "evidence",
                    format_smolstr!("证据写入失败 {}：{error}", path.display()),
                );
                false
            }
        }
    }

    fn capture(&self, app: &mut App, report: &mut RealPixelReport) -> bool {
        let stage = report.stage;
        let Some(logical) = app.window(self.window).map(|target| target.size()) else {
            report.record(false, "window", SmolStr::new_static("截图前窗口消失"));
            return false;
        };
        let dpr = app
            .window(self.window)
            .map_or(1.0, |target| target.device_pixel_ratio().factor());
        let frame = app
            .window(self.window)
            .map_or(0, |target| target.frame_index().get());
        let expected_w = (logical.width.value() * dpr).round().max(1.0) as u32;
        let expected_h = (logical.height.value() * dpr).round().max(1.0) as u32;
        let started = Instant::now();
        let mut shot = match self.screenshot(stage) {
            Ok(shot) => shot,
            Err(error) => {
                report.record(false, "screenshot", error);
                return false;
            }
        };
        let mut attempts = 0u8;
        while attempts < REAL_PIXEL_CAPTURE_RETRIES {
            let found =
                real_pixel_detect_marker(&shot, expected_w, expected_h, RealPixelTone::Full)
                    .is_some()
                    || (stage == RealPixelStage::DialogOpening
                        && real_pixel_detect_marker(
                            &shot,
                            expected_w,
                            expected_h,
                            RealPixelTone::Scrim,
                        )
                        .is_some());
            if found {
                break;
            }
            attempts = attempts.saturating_add(1);
            report.note(format_smolstr!(
                "stage={stage:?} 截图未得到完整标记框（合成撕裂/窗口被遮挡），重试 #{attempts}/{REAL_PIXEL_CAPTURE_RETRIES}"
            ));
            std::thread::sleep(Duration::from_millis(REAL_PIXEL_CAPTURE_RETRY_MS));
            let _ = app.request_repaint(self.window);
            match self.screenshot(stage) {
                Ok(next) => shot = next,
                Err(error) => {
                    report.record(false, "screenshot", error);
                    return false;
                }
            }
        }
        report.screen = shot.dimensions();
        report.window_logical = logical;
        report.dpi = dpr;
        let capture_line = format_smolstr!(
            "capture stage={stage:?} frame={frame} screen={}x{} window={:.0}x{:.0} dpr={dpr:.3} took={} ms frame_cpu={:?} popup_opening={} popup_closing={} primitives={}",
            report.screen.0,
            report.screen.1,
            logical.width.value(),
            logical.height.value(),
            started.elapsed().as_millis(),
            app.window(self.window)
                .and_then(|target| target.frame_stats())
                .and_then(|stats| stats.frame_cpu),
            app.window(self.window)
                .and_then(|target| target.frame_stats())
                .is_some_and(|stats| stats.popup_opening),
            app.window(self.window)
                .and_then(|target| target.frame_stats())
                .is_some_and(|stats| stats.popup_closing),
            app.window(self.window)
                .and_then(|target| target.frame_stats())
                .map_or(0, |stats| stats.primitives),
        );
        report.lines.push(capture_line);
        report.last_frame = frame;
        let mut tone = RealPixelTone::Full;
        let mut marker = real_pixel_detect_marker(&shot, expected_w, expected_h, tone);
        if marker.is_none() && stage == RealPixelStage::DialogOpening {
            tone = RealPixelTone::Scrim;
            marker = real_pixel_detect_marker(&shot, expected_w, expected_h, tone);
        }
        let marker = match marker {
            Some(marker) => Some(marker),
            None => {
                if real_pixel_allows_resize(stage) && report.marker_retries < 8 {
                    report.marker_retries = report.marker_retries.saturating_add(1);
                    report.note(format_smolstr!(
                        "stage={stage:?} 标记尚未按新窗口尺寸出现，重试截图 #{}/8",
                        report.marker_retries
                    ));
                    report.stage_frame = frame;
                    return true;
                }
                let fallback = if stage == RealPixelStage::DialogOpening {
                    self.baseline
                        .borrow()
                        .as_ref()
                        .map(|baseline| RealPixelMarker {
                            origin: baseline.origin,
                            size: baseline.size,
                            thickness: baseline.thickness,
                        })
                } else {
                    None
                };
                if let Some(marker) = fallback {
                    report.note(format_smolstr!(
                        "stage={stage:?} 未检测到 {} 调标记（scrim 遮挡），回退到 baseline 原点 {:?}",
                        match tone {
                            RealPixelTone::Full => "full",
                            RealPixelTone::Scrim => "scrim",
                        },
                        marker.origin
                    ));
                    Some(marker)
                } else {
                    let marker_rect = app
                        .window(self.window)
                        .and_then(|target| target.node_screen_rect(self.marker));
                    let root_rect = app
                        .window(self.window)
                        .and_then(|target| target.node_screen_rect(target.tree().root()));
                    let failure_screen = report.evidence.join(format!(
                        "failed-marker-{}.png",
                        real_pixel_stage_slug(stage)
                    ));
                    let saved = shot.save(&failure_screen).is_ok();
                    report.record(
                        false,
                        "marker",
                        format_smolstr!(
                            "stage={stage:?} tone={tone:?} expected={expected_w}x{expected_h} screen={}x{}：未检测到 #FF00FF {REAL_PIXEL_MARKER_BORDER} px 内容边框标记（窗口被遮挡/不可见/标记被覆盖）；marker_node={marker_rect:?} root={root_rect:?} failure_screen={} saved={saved}",
                            report.screen.0,
                            report.screen.1,
                            failure_screen.display()
                        ),
                    );
                    None
                }
            }
        };
        let Some(marker) = marker else {
            return false;
        };
        let Some(content) = real_pixel_crop(&shot, marker) else {
            report.record(
                false,
                "crop",
                format_smolstr!("标记框 {:?} 超出截图范围 {:?}", marker, report.screen),
            );
            return false;
        };
        match stage {
            RealPixelStage::Warmup => {
                let expected_thickness = (REAL_PIXEL_MARKER_BORDER * dpr).round() as u32;
                let size_tolerance = (dpr * 4.0).round().max(4.0) as u32;
                report.record(
                    true,
                    "marker-detected",
                    format_smolstr!(
                        "tone={tone:?} origin={:?} size={:?} thickness={}",
                        marker.origin,
                        marker.size,
                        marker.thickness
                    ),
                );
                report.record(
                    marker.size.0.abs_diff(expected_w) <= size_tolerance
                        && marker.size.1.abs_diff(expected_h) <= size_tolerance,
                    "content-physical-size",
                    format_smolstr!(
                        "detected={}x{} expected={expected_w}x{expected_h} tolerance={size_tolerance} logical={:.0}x{:.0} dpr={dpr:.3}",
                        marker.size.0,
                        marker.size.1,
                        logical.width.value(),
                        logical.height.value()
                    ),
                );
                report.record(
                    marker.thickness.abs_diff(expected_thickness) <= 3,
                    "marker-thickness",
                    format_smolstr!(
                        "detected={} expected={expected_thickness}（{REAL_PIXEL_MARKER_BORDER} px × dpr）",
                        marker.thickness
                    ),
                );
                report.record(
                    marker.origin.0 < report.screen.0
                        && marker.origin.1 < report.screen.1
                        && marker.origin.0 + marker.size.0 <= report.screen.0
                        && marker.origin.1 + marker.size.1 <= report.screen.1,
                    "marker-on-screen",
                    format_smolstr!(
                        "origin={:?} size={:?} screen={}x{}",
                        marker.origin,
                        marker.size,
                        report.screen.0,
                        report.screen.1
                    ),
                );
                self.save_evidence(report, stage, &content);
                report.warmup = Some(RealPixelBaseline {
                    content,
                    origin: marker.origin,
                    size: marker.size,
                    thickness: marker.thickness,
                    scale: dpr,
                    logical,
                });
                self.advance(app, report, RealPixelStage::Baseline, frame);
                true
            }
            RealPixelStage::Baseline => {
                let (same_marker, stable) = {
                    let Some(warmup) = report.warmup.as_ref() else {
                        report.record(false, "baseline", SmolStr::new_static("warmup 缺失"));
                        return false;
                    };
                    let same_marker = marker.origin == warmup.origin && marker.size == warmup.size;
                    let stable = real_pixel_compare(
                        &warmup.content,
                        &content,
                        real_pixel_content_inner_region(warmup),
                        REAL_PIXEL_RESIDUE_TOLERANCE,
                        None,
                    );
                    (same_marker, stable)
                };
                report.record(
                    same_marker,
                    "marker-stable",
                    format_smolstr!("origin={:?} size={:?}", marker.origin, marker.size),
                );
                report.record(
                    stable.differing == 0,
                    "baseline-stable",
                    format_smolstr!(
                        "compared={} differing={} max_delta={}",
                        stable.compared,
                        stable.differing,
                        stable.max_delta
                    ),
                );
                let whole = RealPixelRegion::new(0, 0, content.width(), content.height());
                let shifted = real_pixel_shifted(&content, 4, 3);
                let shifted_diff = real_pixel_compare(
                    &content,
                    &shifted,
                    whole,
                    REAL_PIXEL_RESIDUE_TOLERANCE,
                    None,
                );
                report.record(
                    shifted_diff.differing > 0,
                    "self-check-offset",
                    format_smolstr!(
                        "r 基线块平移 (4,3) 后检测器必须判红：differing={} max_delta={}",
                        shifted_diff.differing,
                        shifted_diff.max_delta
                    ),
                );
                let block = RealPixelRegion::new(
                    whole.width() / 2,
                    whole.height() / 2,
                    whole.width() / 2 + 48,
                    whole.height() / 2 + 48,
                );
                let recolored = real_pixel_recolored(&content, block);
                let recolor_diff = real_pixel_compare(
                    &content,
                    &recolored,
                    whole,
                    REAL_PIXEL_RESIDUE_TOLERANCE,
                    None,
                );
                report.record(
                    recolor_diff.differing > 0,
                    "self-check-recolor",
                    format_smolstr!(
                        "基线块改色 {:?} 后检测器必须判红：differing={} max_delta={}",
                        block,
                        recolor_diff.differing,
                        recolor_diff.max_delta
                    ),
                );
                let trigger_rect = app
                    .window(self.window)
                    .and_then(|target| target.node_screen_rect(self.trigger));
                *self.baseline.borrow_mut() = Some(RealPixelBaseline {
                    content,
                    origin: marker.origin,
                    size: marker.size,
                    thickness: marker.thickness,
                    scale: dpr,
                    logical,
                });
                let exclusion = {
                    let baseline = self.baseline.borrow();
                    baseline.as_ref().and_then(|baseline| {
                        trigger_rect.and_then(|rect| {
                            real_pixel_map_rect(baseline, rect, REAL_PIXEL_TRIGGER_MARGIN)
                        })
                    })
                };
                report.trigger_exclude = exclusion;
                report.record(
                    exclusion.is_some(),
                    "trigger-exclusion",
                    format_smolstr!(
                        "悬停/焦点敏感触发器 rect={trigger_rect:?} 从全窗残影比较中排除 {exclusion:?}"
                    ),
                );
                {
                    let baseline = self.baseline.borrow();
                    if let Some(baseline) = baseline.as_ref() {
                        self.save_evidence(report, stage, &baseline.content);
                    }
                }
                match self.picker.open(app, self.window) {
                    Ok(true) => report.record(
                        true,
                        "picker-open-call",
                        SmolStr::new_static("ColorPicker::open -> true"),
                    ),
                    Ok(false) => {
                        report.record(
                            false,
                            "picker-open-call",
                            SmolStr::new_static("ColorPicker::open -> false（已打开？）"),
                        );
                        return false;
                    }
                    Err(error) => {
                        report.record(
                            false,
                            "picker-open-call",
                            format_smolstr!("ColorPicker::open 失败：{error}"),
                        );
                        return false;
                    }
                }
                self.advance(app, report, RealPixelStage::PickerOpening, frame);
                true
            }
            RealPixelStage::PickerOpening => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    marker.origin == baseline.origin && marker.size == baseline.size,
                    "marker-stable",
                    format_smolstr!("origin={:?} size={:?}", marker.origin, marker.size),
                );
                let entries = real_pixel_stack_entries(app, self.window);
                let picker_node = self.picker.popup_node();
                let entry = entries.iter().find(|entry| Some(entry.node) == picker_node);
                report.record(
                    entries.len() == 1,
                    "picker-stack-size",
                    format_smolstr!("打开后 popup 栈应有 1 条，实际 {}", entries.len()),
                );
                report.record(
                    entry.is_some_and(|entry| entry.state == ui_core::popup::PopupState::Open),
                    "picker-state",
                    format_smolstr!("{:?}", entry.map(|entry| entry.state)),
                );
                report.record(
                    entry.is_some_and(|entry| {
                        entry.paint_offset.x.value().abs() < 0.5
                            && entry.paint_offset.y.value().abs() < 0.5
                    }),
                    "picker-settled",
                    format_smolstr!("paint_offset={:?}", entry.map(|entry| entry.paint_offset)),
                );
                let Some(entry) = entry else {
                    report.record(
                        false,
                        "picker-entry",
                        SmolStr::new_static("找不到 picker popup 条目"),
                    );
                    return false;
                };
                let rect = entry.paint_rect;
                report.picker_rect = Some(rect);
                report.record(
                    real_pixel_contained(baseline, rect),
                    "picker-in-window",
                    real_pixel_rect_detail(rect),
                );
                real_pixel_visibility_check(
                    report,
                    "picker-open-visible",
                    baseline,
                    &content,
                    rect,
                    REAL_PIXEL_OPEN_PERMILLE,
                );
                self.save_evidence(report, stage, &content);
                let closed = self.picker.close(app);
                report.record(
                    closed,
                    "picker-close-call",
                    format_smolstr!("ColorPicker::close -> {closed}"),
                );
                self.advance(app, report, RealPixelStage::PickerClosing, frame);
                true
            }
            RealPixelStage::PickerClosing => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    real_pixel_stack_entries(app, self.window).is_empty(),
                    "picker-stack-empty",
                    SmolStr::new_static("关闭后 popup 栈必须为空"),
                );
                let Some(rect) = report.picker_rect else {
                    report.record(
                        false,
                        "picker-rect",
                        SmolStr::new_static("缺少 picker rect"),
                    );
                    return false;
                };
                real_pixel_residue_check(report, "picker-close-residue", baseline, &content, rect);
                self.save_evidence(report, stage, &content);
                let anchor = ui_core::popup::Anchor::WindowRect(LogicalRect::from_origin_size(
                    LogicalPoint::new(px(80.0), px(120.0)),
                    LogicalSize::new(px(1.0), px(1.0)),
                ));
                let root = self.chain_menu.open(app, self.window, anchor);
                report.record(
                    matches!(root, Ok(true)),
                    "menu-open-call",
                    format_smolstr!("MenuChain::open(root) -> {root:?}"),
                );
                if !matches!(root, Ok(true)) {
                    return false;
                }
                report.menu_levels = 1;
                self.advance(app, report, RealPixelStage::MenuOpening, frame);
                true
            }
            RealPixelStage::MenuOpening => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    marker.origin == baseline.origin && marker.size == baseline.size,
                    "marker-stable",
                    format_smolstr!("origin={:?} size={:?}", marker.origin, marker.size),
                );
                let entries = real_pixel_stack_entries(app, self.window);
                report.record(
                    entries.len() == 3,
                    "menu-stack-size",
                    format_smolstr!("3 级菜单链应有 3 条 popup，实际 {}", entries.len()),
                );
                report.record(
                    entries.len() == 3
                        && entries
                            .iter()
                            .all(|entry| entry.state == ui_core::popup::PopupState::Open),
                    "menu-state",
                    format_smolstr!(
                        "states={:?}",
                        entries.iter().map(|entry| entry.state).collect::<Vec<_>>()
                    ),
                );
                report.record(
                    entries.len() == 3
                        && entries[0].z < entries[1].z
                        && entries[1].z < entries[2].z,
                    "menu-z-order",
                    format_smolstr!(
                        "z={:?}",
                        entries.iter().map(|entry| entry.z).collect::<Vec<_>>()
                    ),
                );
                report.record(
                    entries.len() == 3
                        && entries[1].parent == Some(entries[0].id)
                        && entries[2].parent == Some(entries[1].id),
                    "menu-parent-chain",
                    format_smolstr!(
                        "parents={:?}",
                        entries.iter().map(|entry| entry.parent).collect::<Vec<_>>()
                    ),
                );
                report.record(
                    entries.len() == 3
                        && entries
                            .iter()
                            .all(|entry| real_pixel_contained(baseline, entry.paint_rect)),
                    "menu-in-window",
                    format_smolstr!(
                        "rects={:?}",
                        entries
                            .iter()
                            .map(|entry| entry.paint_rect)
                            .collect::<Vec<_>>()
                    ),
                );
                report.menu_rects = entries.iter().map(|entry| entry.paint_rect).collect();
                for (index, rect) in report.menu_rects.clone().into_iter().enumerate() {
                    let label = match index {
                        0 => "menu-open-visible-root",
                        1 => "menu-open-visible-level2",
                        _ => "menu-open-visible-level3",
                    };
                    real_pixel_visibility_check(
                        report,
                        label,
                        baseline,
                        &content,
                        rect,
                        REAL_PIXEL_OPEN_PERMILLE,
                    );
                }
                self.save_evidence(report, stage, &content);
                let closed = self.chain_menu.close(app);
                report.record(
                    closed,
                    "menu-close-call",
                    format_smolstr!("MenuChain::close -> {closed}"),
                );
                self.advance(app, report, RealPixelStage::MenuClosing, frame);
                true
            }
            RealPixelStage::MenuClosing => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    real_pixel_stack_entries(app, self.window).is_empty(),
                    "menu-stack-empty",
                    SmolStr::new_static("3 级菜单链关闭后 popup 栈必须为空"),
                );
                let rects = report.menu_rects.clone();
                for (index, rect) in rects.into_iter().enumerate() {
                    let label = match index {
                        0 => "menu-close-residue-root",
                        1 => "menu-close-residue-level2",
                        _ => "menu-close-residue-level3",
                    };
                    real_pixel_residue_check(report, label, baseline, &content, rect);
                }
                self.save_evidence(report, stage, &content);
                match self.dialog.open(app, self.window) {
                    Ok(true) => report.record(
                        true,
                        "dialog-open-call",
                        SmolStr::new_static("Dialog::open -> true"),
                    ),
                    Ok(false) => {
                        report.record(
                            false,
                            "dialog-open-call",
                            SmolStr::new_static("Dialog::open -> false（已打开？）"),
                        );
                        return false;
                    }
                    Err(error) => {
                        report.record(
                            false,
                            "dialog-open-call",
                            format_smolstr!("Dialog::open 失败：{error}"),
                        );
                        return false;
                    }
                }
                self.advance(app, report, RealPixelStage::DialogOpening, frame);
                true
            }
            RealPixelStage::DialogOpening => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                if tone == RealPixelTone::Scrim {
                    report.record(
                        marker.origin == baseline.origin && marker.size == baseline.size,
                        "dialog-marker-under-scrim",
                        format_smolstr!(
                            "scrim 调标记 origin={:?} size={:?} baseline={:?}",
                            marker.origin,
                            marker.size,
                            baseline.origin
                        ),
                    );
                }
                let entries = real_pixel_stack_entries(app, self.window);
                let dialog_node = self.dialog.popup_node();
                let dialog_entry = entries.iter().find(|entry| Some(entry.node) == dialog_node);
                let scrim_entry = entries
                    .iter()
                    .find(|entry| entry.tier == ui_core::popup::PopupTier::Scrim);
                report.record(
                    entries.len() == 2,
                    "dialog-stack-size",
                    format_smolstr!("模态 + scrim 应有 2 条 popup，实际 {}", entries.len()),
                );
                report.record(
                    dialog_entry.is_some_and(|entry| {
                        entry.state == ui_core::popup::PopupState::Open
                            && entry.tier == ui_core::popup::PopupTier::Modal
                    }),
                    "dialog-state",
                    format_smolstr!(
                        "dialog={:?}",
                        dialog_entry.map(|entry| (entry.state, entry.tier))
                    ),
                );
                report.record(
                    scrim_entry
                        .is_some_and(|entry| entry.state == ui_core::popup::PopupState::Open),
                    "dialog-scrim-state",
                    format_smolstr!("scrim={:?}", scrim_entry.map(|entry| entry.state)),
                );
                let Some(dialog_entry) = dialog_entry else {
                    report.record(
                        false,
                        "dialog-entry",
                        SmolStr::new_static("找不到 dialog popup 条目"),
                    );
                    return false;
                };
                let panel = dialog_entry.paint_rect;
                report.dialog_panel = Some(panel);
                report.record(
                    real_pixel_contained(baseline, panel),
                    "dialog-in-window",
                    real_pixel_rect_detail(panel),
                );
                if let Some(scrim_entry) = scrim_entry {
                    report.scrim_rect = Some(scrim_entry.paint_rect);
                    let scrim_covers = real_pixel_map_rect(baseline, scrim_entry.paint_rect, 0)
                        .is_some_and(|region| {
                            region.x0 <= 2
                                && region.y0 <= 2
                                && region.x1 + 2 >= baseline.size.0
                                && region.y1 + 2 >= baseline.size.1
                        });
                    report.record(
                        scrim_covers,
                        "dialog-scrim-covers-content",
                        format_smolstr!(
                            "scrim={} content={}x{}",
                            real_pixel_rect_detail(scrim_entry.paint_rect),
                            baseline.size.0,
                            baseline.size.1
                        ),
                    );
                }
                real_pixel_visibility_check(
                    report,
                    "dialog-panel-visible",
                    baseline,
                    &content,
                    panel,
                    REAL_PIXEL_DIALOG_PANEL_PERMILLE,
                );
                let whole = real_pixel_content_region(baseline);
                let scrim = report.scrim;
                let live_scrim = real_pixel_theme_scrim(&self.theme);
                report.record(
                    live_scrim == scrim,
                    "dialog-scrim-theme",
                    format_smolstr!("运行中主题 scrim 未变化：build={scrim:?} live={live_scrim:?}"),
                );
                let predicted = real_pixel_scrim_predicted(&baseline.content, scrim);
                let shadow_margin =
                    (REAL_PIXEL_SCRIM_PANEL_MARGIN as f32 * baseline.scale).round() as u32;
                let panel_exclusion = real_pixel_map_rect(baseline, panel, shadow_margin);
                let edge_margin = ((REAL_PIXEL_MARKER_BORDER + 4.0) * baseline.scale)
                    .round()
                    .max(REAL_PIXEL_SCRIM_EDGE_MARGIN as f32)
                    as u32;
                let inner = RealPixelRegion::new(
                    edge_margin,
                    edge_margin,
                    whole.x1.saturating_sub(edge_margin),
                    whole.y1.saturating_sub(edge_margin),
                );
                let mut exclusions: Vec<RealPixelRegion> = Vec::with_capacity(2);
                if let Some(panel_exclusion) = panel_exclusion {
                    exclusions.push(panel_exclusion);
                }
                #[cfg(target_os = "macos")]
                {
                    let width = (160.0 * baseline.scale).round() as u32;
                    let height = (48.0 * baseline.scale).round() as u32;
                    exclusions.push(RealPixelRegion::new(
                        0,
                        0,
                        width.min(whole.x1),
                        height.min(whole.y1),
                    ));
                }
                let (loose, strict) =
                    real_pixel_scrim_analysis(&predicted, &content, inner, &exclusions);
                let outlier_permille = loose
                    .differing
                    .saturating_mul(1000)
                    .checked_div(loose.compared.max(1))
                    .unwrap_or(0);
                report.record(
                    loose.compared >= 500_000
                        && outlier_permille <= REAL_PIXEL_SCRIM_OUTLIER_PERMILLE
                        && strict.differing == 0
                        && strict.max_delta <= 24,
                    "dialog-scrim-composite",
                    format_smolstr!(
                        "scrim={:?} alpha={:.4} predicted-from-baseline: compared={} outliers>4={outlier_permille}‰ outliers>12={} max_delta={} exclusions={exclusions:?}",
                        [scrim[0], scrim[1], scrim[2]],
                        scrim[3],
                        loose.compared,
                        strict.differing,
                        strict.max_delta
                    ),
                );
                let derived = real_pixel_derive_scrim_alpha(baseline, &content, scrim);
                report.record(
                    derived.is_some_and(|(alpha, samples)| {
                        samples >= 1000
                            && (alpha - scrim[3]).abs() <= REAL_PIXEL_SCRIM_ALPHA_TOLERANCE
                    }),
                    "dialog-scrim-alpha",
                    format_smolstr!(
                        "theme_alpha={:.4} derived_from_marker={derived:?} tolerance={REAL_PIXEL_SCRIM_ALPHA_TOLERANCE}",
                        scrim[3]
                    ),
                );
                let panel_region = real_pixel_map_rect(baseline, panel, 2);
                let Some(panel_region) = panel_region else {
                    report.record(
                        false,
                        "dialog-panel-above-scrim",
                        SmolStr::new_static("panel rect 无法映射到内容区"),
                    );
                    return false;
                };
                let above = real_pixel_compare(
                    &predicted,
                    &content,
                    panel_region,
                    REAL_PIXEL_OPEN_TOLERANCE,
                    None,
                );
                let above_permille = above
                    .compared
                    .saturating_mul(1000)
                    .checked_div(panel_region.area())
                    .unwrap_or(0);
                report.record(
                    above_permille >= 500 && above.max_delta >= REAL_PIXEL_VISIBLE_DELTA,
                    "dialog-panel-above-scrim",
                    format_smolstr!(
                        "panel={panel_region:?} changed_vs_scrim_composite={} ({above_permille}‰) max_delta={}",
                        above.differing,
                        above.max_delta
                    ),
                );
                let mut scrim_fault = content.clone();
                for y in whole.y0..whole.y1 {
                    for x in whole.x0..whole.x1 {
                        scrim_fault.put_pixel(x, y, *baseline.content.get_pixel(x, y));
                    }
                }
                let (fault_loose, _) =
                    real_pixel_scrim_analysis(&predicted, &scrim_fault, inner, &exclusions);
                let fault_alpha = real_pixel_derive_scrim_alpha(baseline, &scrim_fault, scrim);
                let scrim_fault_detected = fault_loose.differing > 0
                    || !fault_alpha.is_some_and(|(alpha, _)| {
                        (alpha - scrim[3]).abs() <= REAL_PIXEL_SCRIM_ALPHA_TOLERANCE
                    });
                report.record(
                    scrim_fault_detected,
                    "self-check-scrim-fault",
                    format_smolstr!(
                        "把 scrim 区域替换回 baseline 后检测器必须判红：composite_outliers={} derived={fault_alpha:?}",
                        fault_loose.differing
                    ),
                );
                let mut panel_fault = content.clone();
                for y in panel_region.y0..panel_region.y1 {
                    for x in panel_region.x0..panel_region.x1 {
                        panel_fault.put_pixel(x, y, *predicted.get_pixel(x, y));
                    }
                }
                let above_fault = real_pixel_compare(
                    &predicted,
                    &panel_fault,
                    panel_region,
                    REAL_PIXEL_OPEN_TOLERANCE,
                    None,
                );
                let above_fault_permille = above_fault
                    .differing
                    .saturating_mul(1000)
                    .checked_div(panel_region.area())
                    .unwrap_or(0);
                report.record(
                    above_fault_permille < 500,
                    "self-check-panel-fault",
                    format_smolstr!(
                        "把 panel 区域替换为 scrim 合成后检测器必须判红：changed={above_fault_permille}‰ (需 <500)"
                    ),
                );
                let diff = real_pixel_compare(
                    &baseline.content,
                    &content,
                    whole,
                    REAL_PIXEL_OPEN_TOLERANCE,
                    None,
                );
                let permille = diff
                    .compared
                    .saturating_mul(1000)
                    .checked_div(whole.area())
                    .unwrap_or(0);
                report.record(
                    permille >= REAL_PIXEL_SCRIM_PERMILLE
                        && diff.max_delta >= REAL_PIXEL_VISIBLE_DELTA,
                    "dialog-scrim-visible",
                    format_smolstr!(
                        "content={whole:?} changed={} ({permille}‰) max_delta={}",
                        diff.differing,
                        diff.max_delta
                    ),
                );
                self.save_evidence(report, stage, &content);
                let closed = self.dialog.close(app);
                report.record(
                    closed,
                    "dialog-close-call",
                    format_smolstr!("Dialog::close -> {closed}"),
                );
                self.advance(app, report, RealPixelStage::DialogClosing, frame);
                true
            }
            RealPixelStage::DialogClosing => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    marker.origin == baseline.origin && marker.size == baseline.size,
                    "dialog-close-marker-restored",
                    format_smolstr!(
                        "tone={tone:?} origin={:?} size={:?} baseline={:?}",
                        marker.origin,
                        marker.size,
                        baseline.origin
                    ),
                );
                report.record(
                    real_pixel_stack_entries(app, self.window).is_empty(),
                    "dialog-stack-empty",
                    SmolStr::new_static("Dialog 关闭后 popup 栈必须为空（含 scrim）"),
                );
                real_pixel_content_residue_check(
                    report,
                    "dialog-close-residue",
                    baseline,
                    &content,
                );
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractPointerOpen, frame);
                true
            }
            RealPixelStage::InteractPointerOpen => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    marker.origin == baseline.origin && marker.size == baseline.size,
                    "interact-marker-stable",
                    format_smolstr!("origin={:?} size={:?}", marker.origin, marker.size),
                );
                let entries = self.interact_entries(app);
                report.record(
                    entries.len() == 1
                        && entries
                            .first()
                            .is_some_and(|entry| entry.state == ui_core::popup::PopupState::Open),
                    "interact-open-stack",
                    format_smolstr!(
                        "真实指针点击触发器后 popup 栈应有 1 条 Open，实际 {:?}",
                        entries
                            .iter()
                            .map(|entry| (entry.state, entry.tier))
                            .collect::<Vec<_>>()
                    ),
                );
                report.record(
                    self.chain_menu.is_open(app),
                    "interact-open-menu",
                    SmolStr::new_static(
                        "Menu::is_open 必须为 true（指针点击打开，而非程序化 open_submenu）",
                    ),
                );
                if let Some(entry) = entries.first() {
                    report.interact_root_node = Some(entry.node);
                    report.interact_root_rect = Some(entry.paint_rect);
                    report.record(
                        real_pixel_contained(baseline, entry.paint_rect),
                        "interact-open-in-window",
                        real_pixel_rect_detail(entry.paint_rect),
                    );
                    real_pixel_visibility_check(
                        report,
                        "interact-open-visible",
                        baseline,
                        &content,
                        entry.paint_rect,
                        REAL_PIXEL_INTERACT_PERMILLE,
                    );
                }
                report.interact_open_content = Some(content.clone());
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractHover, frame);
                true
            }
            RealPixelStage::InteractHover => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                let entries = self.interact_entries(app);
                report.record(
                    entries.len() == 2
                        && entries
                            .iter()
                            .all(|entry| entry.state == ui_core::popup::PopupState::Open),
                    "hover-opens-submenu",
                    format_smolstr!(
                        "指针悬停二级行后应 hover 打开子菜单（stack=2），实际 {:?}",
                        entries.iter().map(|entry| entry.state).collect::<Vec<_>>()
                    ),
                );
                report.record(
                    entries.len() == 2
                        && entries[1].parent == Some(entries[0].id)
                        && entries[0].z < entries[1].z,
                    "hover-parent-chain",
                    format_smolstr!(
                        "parents={:?} z={:?}",
                        entries.iter().map(|entry| entry.parent).collect::<Vec<_>>(),
                        entries.iter().map(|entry| entry.z).collect::<Vec<_>>()
                    ),
                );
                let highlighted = self.chain_menu.highlighted(app);
                report.record(
                    highlighted == Some(2),
                    "hover-row-highlight",
                    format_smolstr!("悬停行高亮应为二级行 2，实际 {highlighted:?}"),
                );
                if let Some(entry) = entries.get(1) {
                    report.interact_level2_node = Some(entry.node);
                    report.interact_level2_rect = Some(entry.paint_rect);
                    real_pixel_visibility_check(
                        report,
                        "hover-level2-visible",
                        baseline,
                        &content,
                        entry.paint_rect,
                        REAL_PIXEL_INTERACT_PERMILLE,
                    );
                }
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractSafeTriangle, frame);
                true
            }
            RealPixelStage::InteractSafeTriangle => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                let entries = self.interact_entries(app);
                report.record(
                    entries.len() == 3
                        && entries
                            .iter()
                            .all(|entry| entry.state == ui_core::popup::PopupState::Open),
                    "safe-triangle-level3",
                    format_smolstr!(
                        "安全三角内斜向移动到三级行后应保持 3 层并打开三级，实际 {} 层",
                        entries.len()
                    ),
                );
                report.record(
                    entries.len() == 3
                        && entries[2].parent == Some(entries[1].id)
                        && entries[1].parent == Some(entries[0].id),
                    "safe-triangle-chain",
                    format_smolstr!(
                        "parents={:?}",
                        entries.iter().map(|entry| entry.parent).collect::<Vec<_>>()
                    ),
                );
                if let Some(entry) = entries.get(2) {
                    real_pixel_visibility_check(
                        report,
                        "safe-triangle-level3-visible",
                        baseline,
                        &content,
                        entry.paint_rect,
                        REAL_PIXEL_INTERACT_PERMILLE,
                    );
                }
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractSibling, frame);
                true
            }
            RealPixelStage::InteractSibling => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                let entries = self.interact_entries(app);
                let sibling = entries.get(1).map(|entry| (entry.node, entry.paint_rect));
                report.record(
                    entries.len() == 2,
                    "sibling-stack",
                    format_smolstr!(
                        "兄弟行替换后应保持 2 层（根+新二级），实际 {}",
                        entries.len()
                    ),
                );
                let replaced =
                    sibling.is_some_and(|(node, _)| Some(node) != report.interact_level2_node);
                report.record(
                    replaced,
                    "sibling-replaced",
                    format_smolstr!(
                        "旧二级 node={:?} 新二级 node={:?}",
                        report.interact_level2_node,
                        sibling.map(|(node, _)| node)
                    ),
                );
                if let Some((node, rect)) = sibling {
                    report.interact_sibling_node = Some(node);
                    report.interact_sibling_rect = Some(rect);
                    real_pixel_visibility_check(
                        report,
                        "sibling-visible",
                        baseline,
                        &content,
                        rect,
                        REAL_PIXEL_INTERACT_PERMILLE,
                    );
                }
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractEscape, frame);
                true
            }
            RealPixelStage::InteractEscape => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    real_pixel_stack_entries(app, self.window).is_empty(),
                    "escape-stack-empty",
                    SmolStr::new_static("逐层 Esc 后 popup 栈必须为空"),
                );
                report.record(
                    !self.chain_menu.is_open(app),
                    "escape-menu-closed",
                    SmolStr::new_static("逐层 Esc 后 Menu::is_open 必须为 false"),
                );
                real_pixel_content_residue_check(
                    report,
                    "interact-close-residue",
                    baseline,
                    &content,
                );
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractOutsideReplay, frame);
                true
            }
            RealPixelStage::InteractOutsideReplay => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    real_pixel_stack_entries(app, self.window).is_empty()
                        && !self.chain_menu.is_open(app),
                    "outside-close",
                    SmolStr::new_static("外部点击后 popup 栈为空且菜单关闭"),
                );
                let replay_on = app.read_with(&self.replay_on, |value| *value);
                report.interact_replay_on = replay_on;
                report.record(
                    replay_on,
                    "outside-replay-signal",
                    format_smolstr!("Replay 到背景控件后 replay_on 必须为 true，实际 {replay_on}"),
                );
                let replay_rect = LogicalRect::new(
                    px(REAL_PIXEL_INTERACT_REPLAY.0),
                    px(REAL_PIXEL_INTERACT_REPLAY.1),
                    px(REAL_PIXEL_INTERACT_REPLAY.2),
                    px(REAL_PIXEL_INTERACT_REPLAY.3),
                );
                if let Some(region) = real_pixel_map_rect(baseline, replay_rect, 2) {
                    let (diff, permille) = real_pixel_region_change(
                        &baseline.content,
                        &content,
                        region,
                        REAL_PIXEL_OPEN_TOLERANCE,
                    );
                    report.record(
                        permille >= REAL_PIXEL_INTERACT_PERMILLE
                            && diff.max_delta >= REAL_PIXEL_VISIBLE_DELTA,
                        "outside-replay-visible",
                        format_smolstr!(
                            "Replay 背景控件区域必须可见变化：changed={permille}‰ max_delta={}",
                            diff.max_delta
                        ),
                    );
                }
                let mut exclusions: Vec<RealPixelRegion> = Vec::new();
                if let Some(region) = report.trigger_exclude {
                    exclusions.push(region);
                }
                for node in [
                    self.interact_trigger,
                    self.replay_node,
                    self.status_node,
                    self.scroll_viewport,
                ] {
                    if let Some(region) =
                        real_pixel_node_region(baseline, app, self.window, node, 4)
                    {
                        exclusions.push(region);
                    }
                }
                let inner = real_pixel_content_inner_region(baseline);
                let diff = real_pixel_compare_multi(
                    &baseline.content,
                    &content,
                    inner,
                    REAL_PIXEL_RESIDUE_TOLERANCE,
                    &exclusions,
                );
                report.record(
                    diff.differing == 0,
                    "outside-close-residue",
                    format_smolstr!(
                        "外部点击关闭后除背景控件外不得有残影：compared={} differing={} max_delta={}",
                        diff.compared,
                        diff.differing,
                        diff.max_delta
                    ),
                );
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractKeyboard, frame);
                true
            }
            RealPixelStage::InteractKeyboard => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    real_pixel_stack_entries(app, self.window).is_empty()
                        && !self.chain_menu.is_open(app),
                    "keyboard-menu-closed",
                    SmolStr::new_static("键盘 Enter 激活条目后菜单必须关闭"),
                );
                let status_on = app.read_with(&self.status_on, |value| *value);
                report.interact_status_on = status_on;
                let activation = self.activated.borrow().clone();
                report.record(
                    status_on && activation.as_deref().is_some(),
                    "keyboard-activation-signal",
                    format_smolstr!("激活回调 status_on={status_on} activation={activation:?}"),
                );
                let status_rect = LogicalRect::new(
                    px(REAL_PIXEL_INTERACT_STATUS.0),
                    px(REAL_PIXEL_INTERACT_STATUS.1),
                    px(REAL_PIXEL_INTERACT_STATUS.2),
                    px(REAL_PIXEL_INTERACT_STATUS.3),
                );
                if let Some(region) = real_pixel_map_rect(baseline, status_rect, 2) {
                    let (diff, permille) = real_pixel_region_change(
                        &baseline.content,
                        &content,
                        region,
                        REAL_PIXEL_OPEN_TOLERANCE,
                    );
                    report.record(
                        permille >= REAL_PIXEL_INTERACT_PERMILLE
                            && diff.max_delta >= REAL_PIXEL_VISIBLE_DELTA,
                        "keyboard-status-visible",
                        format_smolstr!(
                            "键盘激活后的状态块必须可见变化：changed={permille}‰ max_delta={}",
                            diff.max_delta
                        ),
                    );
                }
                let mut exclusions: Vec<RealPixelRegion> = Vec::new();
                if let Some(region) = report.trigger_exclude {
                    exclusions.push(region);
                }
                for node in [
                    self.interact_trigger,
                    self.replay_node,
                    self.status_node,
                    self.scroll_viewport,
                ] {
                    if let Some(region) =
                        real_pixel_node_region(baseline, app, self.window, node, 4)
                    {
                        exclusions.push(region);
                    }
                }
                let inner = real_pixel_content_inner_region(baseline);
                let diff = real_pixel_compare_multi(
                    &baseline.content,
                    &content,
                    inner,
                    REAL_PIXEL_RESIDUE_TOLERANCE,
                    &exclusions,
                );
                report.record(
                    diff.differing == 0,
                    "keyboard-close-residue",
                    format_smolstr!(
                        "键盘路径关闭后除状态块外不得有残影：compared={} differing={} max_delta={}",
                        diff.compared,
                        diff.differing,
                        diff.max_delta
                    ),
                );
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractResizeBaseline, frame);
                true
            }
            RealPixelStage::InteractScrollOpen => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                let entries = self.interact_entries(app);
                report.record(
                    entries.len() == 1,
                    "scroll-open-stack",
                    format_smolstr!("滚动锚点打开菜单 stack={}", entries.len()),
                );
                if let Some(entry) = entries.first() {
                    real_pixel_visibility_check(
                        report,
                        "scroll-open-visible",
                        baseline,
                        &content,
                        entry.paint_rect,
                        REAL_PIXEL_INTERACT_PERMILLE,
                    );
                }
                report.record(
                    report.scroll_delta.abs() > 1.0,
                    "scroll-delta-recorded",
                    format_smolstr!("滚动后弹层 Δy={:.1}", report.scroll_delta),
                );
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractScrollRestore, frame);
                true
            }
            RealPixelStage::InteractScrollRestore => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                report.record(
                    real_pixel_stack_entries(app, self.window).is_empty(),
                    "scroll-close-stack-empty",
                    SmolStr::new_static("滚动还原并关闭菜单后 popup 栈必须为空"),
                );
                report.record(
                    self.scroll_offset(app)
                        .is_none_or(|offset| offset.y.value().abs() < 0.5),
                    "scroll-restored",
                    format_smolstr!("滚动偏移必须回到 0：{:?}", self.scroll_offset(app)),
                );
                let mut exclusions: Vec<RealPixelRegion> = Vec::new();
                if let Some(region) = report.trigger_exclude {
                    exclusions.push(region);
                }
                for node in [self.replay_node, self.status_node] {
                    if let Some(region) =
                        real_pixel_node_region(baseline, app, self.window, node, 4)
                    {
                        exclusions.push(region);
                    }
                }
                let inner = real_pixel_content_inner_region(baseline);
                let diff = real_pixel_compare_multi(
                    &baseline.content,
                    &content,
                    inner,
                    REAL_PIXEL_RESIDUE_TOLERANCE,
                    &exclusions,
                );
                report.record(
                    diff.differing == 0,
                    "scroll-close-residue",
                    format_smolstr!(
                        "滚动还原后除持久状态块外内容必须与原始基线一致：differing={} max_delta={}",
                        diff.differing,
                        diff.max_delta
                    ),
                );
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::InteractSelfChecks, frame);
                true
            }
            RealPixelStage::InteractSelfChecks => {
                let baseline_guard = self.baseline.borrow();
                let Some(baseline) = baseline_guard.as_ref() else {
                    report.record(false, "baseline", SmolStr::new_static("baseline 缺失"));
                    return false;
                };
                let static_reject = report.interact_root_rect.is_some_and(|rect| {
                    !real_pixel_visible_change(
                        baseline,
                        &baseline.content,
                        rect,
                        REAL_PIXEL_INTERACT_PERMILLE,
                    )
                });
                report.record(
                    static_reject,
                    "self-check-visible-static",
                    SmolStr::new_static(
                        "未变化的基线当作“打开”输入时可见性检测器必须判红（避免恒真）",
                    ),
                );
                let residue_detected = report.interact_open_content.as_ref().is_some_and(|open| {
                    let inner = real_pixel_content_inner_region(baseline);
                    real_pixel_compare(
                        &baseline.content,
                        open,
                        inner,
                        REAL_PIXEL_RESIDUE_TOLERANCE,
                        None,
                    )
                    .differing
                        > 0
                });
                report.record(
                    residue_detected,
                    "self-check-residue-open",
                    SmolStr::new_static(
                        "把“打开帧”当作关闭残影输入时残影检测器必须判红（differing>0）",
                    ),
                );
                let entries = real_pixel_stack_entries(app, self.window);
                let level_ok = |expected: usize| {
                    entries.len() == expected
                        && entries
                            .iter()
                            .all(|entry| entry.state == ui_core::popup::PopupState::Open)
                };
                report.record(
                    !level_ok(1) && level_ok(0),
                    "self-check-level-count",
                    format_smolstr!(
                        "层数断言必须区分 0/1：实际 {} 层，错误期望 1 -> {}, 正确期望 0 -> {}",
                        entries.len(),
                        level_ok(1),
                        level_ok(0)
                    ),
                );
                let replay_rect = LogicalRect::new(
                    px(REAL_PIXEL_INTERACT_REPLAY.0),
                    px(REAL_PIXEL_INTERACT_REPLAY.1),
                    px(REAL_PIXEL_INTERACT_REPLAY.2),
                    px(REAL_PIXEL_INTERACT_REPLAY.3),
                );
                let replay_static = real_pixel_map_rect(baseline, replay_rect, 2).map(|region| {
                    let (diff, permille) = real_pixel_region_change(
                        &baseline.content,
                        &baseline.content,
                        region,
                        REAL_PIXEL_OPEN_TOLERANCE,
                    );
                    (permille, diff.compared, diff.differing, diff.max_delta)
                });
                let static_ok = replay_static
                    .is_some_and(|(permille, _, _, _)| permille < REAL_PIXEL_INTERACT_PERMILLE);
                report.record(
                    static_ok,
                    "self-check-replay-static",
                    format_smolstr!(
                        "未变化的背景控件区域不得被判定为 Replay 可见变化：stat={replay_static:?}"
                    ),
                );
                self.save_evidence(report, stage, &content);
                self.advance(app, report, RealPixelStage::Done, frame);
                true
            }
            RealPixelStage::InteractResizeBaseline => true,
            RealPixelStage::Done => true,
        }
    }

    fn finish(&self, app: &mut App) {
        let pass = {
            let mut report = self.report.borrow_mut();
            if report.completed {
                return;
            }
            report.completed = true;
            if let Err(error) = real_pixel_write_report(&report) {
                report.report_write_failed = true;
                report
                    .failures
                    .push(format_smolstr!("report-write: {error}"));
                println!("gallery: real-pixel-check FAIL report write: {error}");
            }
            let elapsed = report
                .started
                .map_or(Duration::ZERO, |started| started.elapsed());
            println!(
                "gallery: real-pixel-check summary platform={} checks={} failures={} elapsed={} ms frame={} revision={} status={} diff_sha={} identity={} profile={} evidence={}",
                report.platform,
                report.checks,
                report.failures.len(),
                elapsed.as_millis(),
                report.last_frame,
                report.source_revision,
                report.source_dirty,
                report.source_diff_sha,
                report.source_identity,
                report.build_profile,
                report.evidence.display()
            );
            report.completed && report.failures.is_empty() && !report.report_write_failed
        };
        let _ = std::io::Write::flush(&mut std::io::stdout());
        app.close_window(self.window);
        app.handle().wake();
        std::process::exit(if pass { 0 } else { 3 });
    }

    fn final_report(&self) -> bool {
        let report = self.report.borrow();
        let passed = report.completed && report.failures.is_empty() && !report.report_write_failed;
        if !report.completed {
            println!(
                "gallery: real-pixel-check FAIL automation did not finish (window closed before the final round; evidence {})",
                report.evidence.display()
            );
            return false;
        }
        if passed {
            println!(
                "gallery: real-pixel-check PASS ({} checks; evidence {}; exit 0)",
                report.checks,
                report.evidence.display()
            );
        } else {
            for failure in &report.failures {
                println!("gallery: real-pixel-check failure {failure}");
            }
            println!(
                "gallery: real-pixel-check FAIL ({} failure(s) in {} checks; evidence {}; exit 3)",
                report.failures.len(),
                report.checks,
                report.evidence.display()
            );
        }
        passed
    }
}

fn real_pixel_write_report(report: &RealPixelReport) -> Result<(), String> {
    use std::fmt::Write as _;
    let path = report.evidence.join("report.txt");
    let mut text = String::with_capacity(16384);
    let _ = writeln!(text, "platform={}", report.platform);
    let _ = writeln!(text, "source_revision={}", report.source_revision);
    let _ = writeln!(text, "source_status={}", report.source_dirty);
    let _ = writeln!(text, "source_diff_sha={}", report.source_diff_sha);
    let _ = writeln!(text, "source_identity={}", report.source_identity);
    let _ = writeln!(text, "build_profile={}", report.build_profile);
    let _ = writeln!(text, "capture_command={}", report.capture_command);
    let _ = writeln!(text, "screen={}x{}", report.screen.0, report.screen.1);
    let _ = writeln!(
        text,
        "window_logical={:.1}x{:.1}",
        report.window_logical.width.value(),
        report.window_logical.height.value()
    );
    let _ = writeln!(text, "dpi_scale={:.4}", report.dpi);
    let _ = writeln!(
        text,
        "scrim_color_linear=[{:.4}, {:.4}, {:.4}] scrim_alpha={:.4}",
        report.scrim[0], report.scrim[1], report.scrim[2], report.scrim[3]
    );
    let _ = writeln!(
        text,
        "last_frame={} elapsed_ms={}",
        report.last_frame,
        report
            .started
            .map_or(Duration::ZERO, |started| started.elapsed())
            .as_millis()
    );
    let _ = writeln!(
        text,
        "damage_full=unavailable(real-window; the headless --pixel-check gate asserts damage_full_frames=0)"
    );
    let _ = writeln!(
        text,
        "residue_rule=tolerance {REAL_PIXEL_RESIDUE_TOLERANCE}, coverage>={REAL_PIXEL_COVERAGE_PERMILLE}permille, trigger-exclusion {REAL_PIXEL_TRIGGER_MARGIN}px"
    );
    let _ = writeln!(
        text,
        "interact_root_rect={:?} level2_rect={:?} sibling_rect={:?}",
        report.interact_root_rect, report.interact_level2_rect, report.interact_sibling_rect
    );
    let _ = writeln!(
        text,
        "interact_activation={:?} highlighted={:?} replay_on={} status_on={}",
        report.interact_activation,
        report.interact_highlighted,
        report.interact_replay_on,
        report.interact_status_on
    );
    let _ = writeln!(
        text,
        "resize_root_rect={:?} scroll_delta={:.1}",
        report.resize_root_rect, report.scroll_delta
    );
    let _ = writeln!(
        text,
        "resize_note=anchor re-anchor is asserted via popup rects after logical resize; the embedded host does not propagate Window::set_size to the native window, so no resize screenshot is taken and pixel restoration is proven by the following scroll/self-check frames"
    );
    let _ = writeln!(text, "checks={}", report.checks);
    let _ = writeln!(text, "failures={}", report.failures.len());
    let result = if report.completed && report.failures.is_empty() && !report.report_write_failed {
        "PASS"
    } else {
        "FAIL"
    };
    let _ = writeln!(text, "result={result}");
    let _ = writeln!(text, "evidence_dir={}", report.evidence.display());
    let _ = writeln!(
        text,
        "evidence_layout=<evidence_dir>/00-warmup.png,00-baseline.png,01-picker-open.png,02-picker-closed.png,03-menu-open.png,04-menu-closed.png,05-dialog-open.png,06-dialog-closed.png,07-interact-open.png,08-interact-hover.png,09-interact-safe-triangle.png,10-interact-sibling.png,11-interact-esc-closed.png,12-interact-outside-replay.png,13-interact-keyboard.png,15-scroll-reanchored.png,16-scroll-restored.png,17-self-checks.png,report.txt,run.log"
    );
    for line in &report.lines {
        let _ = writeln!(text, "{line}");
    }
    std::fs::write(&path, text)
        .map_err(|error| format!("{}: {error}", path.display()))
        .map(|()| {
            println!(
                "gallery: real-pixel-check report written {}",
                path.display()
            );
        })
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flags = parse_flags();
    if flags.pixel_check {
        if !run_pixel_check() {
            std::process::exit(3);
        }
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    if flags.real_pixel_check {
        let acceptance = RealPixelAcceptance::build(&mut app);
        acceptance.install(&mut app);
        Application::new().run(app)?;
        println!("gallery: real-pixel-check event loop exited");
        if !acceptance.final_report() {
            std::process::exit(3);
        }
        std::process::exit(0);
    }
    let gallery = Gallery::build(&mut app, flags);
    println!(
        "gallery: window {:?} created ({}×{} logical), 15 sections",
        gallery.window, WINDOW_W, WINDOW_H
    );
    println!(
        "gallery: gates: --pixel-check = headless pixel gate (exit 0/3); --debug-stack = real-window popup stack/rect gate (exit 0/3 when its final round completes; --auto-close only raises its >=30s watchdog, covers the removed --dump-overlay); --real-pixel-check = true-window full-screen screenshot pixel gate (exit 0/3, evidence under target/real-pixels/<os>/); note: under a busy/concurrent desktop (screenshot tooling, parallel gates, IDE load) --debug-stack can exceed 300 s, the acceptance scripts enforce a 300 s timeout and retry up to 3 attempts — a single timeout is retried and does not change the gate result"
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
    if gallery.flags.theme_check {
        let report = gallery.theme_check.borrow();
        let failures = report.failures.len();
        match report.pass {
            Some(true) => println!("gallery: theme-check PASS ({failures} failure(s))"),
            Some(false) => {
                println!("gallery: theme-check FAIL ({failures} failure(s), see lines above)");
                failed = true;
            }
            None => {
                println!("gallery: theme-check FAIL (report never produced)");
                failed = true;
            }
        }
    }
    if gallery.flags.debug_stack {
        let report = gallery.debug_stack.borrow();
        if report.pass() {
            println!(
                "gallery: overlay-check PASS (menu={:?} menu_frames={} picker_open={} picker_frames={} picker_children={:?})",
                report.menu,
                report.menu_frames,
                report.picker_open,
                report.picker_frames,
                report.picker_panel_children
            );
        } else {
            println!(
                "gallery: overlay-check FAIL ({} failure(s)): {:?}",
                report.failures.len(),
                report.failures
            );
            failed = true;
        }
    }
    if failed {
        std::process::exit(3);
    }
    std::process::exit(0)
}

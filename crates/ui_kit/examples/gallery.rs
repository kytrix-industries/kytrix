use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::{App, Application},
    event::{Action, ActionSource, Event, InputEvent},
    interaction::ScrollConfig,
    math::{DeviceRect, LogicalPoint, LogicalRect, LogicalSize, px},
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
        layer::{Band, Layer},
        list::{List, ListOptions, SelectionMode},
        markdown::{parse::parse_document, source::MarkdownAssets, view::MarkdownNode},
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
    dump_overlay: bool,
    pixel_check: bool,
    scroll_to: Option<f32>,
}

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
            flags.auto_close = value.parse::<u64>().ok();
        } else if arg == "--probe" {
            flags.probe = true;
        } else if arg == "--dump-overlay" {
            flags.dump_overlay = true;
        } else if arg == "--pixel-check" {
            flags.pixel_check = true;
        } else if let Some(value) = arg.strip_prefix("--scroll-to=") {
            flags.scroll_to = value.parse::<f32>().ok();
        } else {
            println!("gallery: ignoring unknown argument {arg}");
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
struct OverlayCheckReport {
    menu: Option<(usize, f32, f32)>,
    picker_open: bool,
    picker_panel_children: Option<usize>,
}

impl OverlayCheckReport {
    fn pass(&self) -> bool {
        let menu_ok = self
            .menu
            .is_some_and(|(children, content, thumb)| children > 0 && content > 0.0 && thumb > 0.0);
        menu_ok
            && self.picker_open
            && self
                .picker_panel_children
                .is_some_and(|children| children > 0)
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
    overlay_check: Rc<RefCell<OverlayCheckReport>>,
    dropdown: Dropdown,
    select: Select,
    menu: Menu,
    context_menu: ContextMenu,
    tooltip: Tooltip,
    overlay: Overlay,
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
        let layer = Layer::new();
        let router = AnimationRouter::new();
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
            "⑫ 浮层（Overlay / Tooltip / Dropdown / Select / Menu / ContextMenu）",
            H_OVERLAYS,
            &theme,
        );
        let overlay_row = row(app, window, s12, 10.0);

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
            let panel_theme = theme.clone();
            move |ctx| {
                let node = ctx.node();
                let window = ctx.window();
                let app = ctx.app_mut();
                let anchor = Anchor::node(node);
                let theme = panel_theme.clone();
                let opened = overlay.open(app, window, anchor, move |tree, panel| {
                    NodeBuilder::text(
                        "Overlay 面板（12 向 placement + flip/clamp + 淡入淡出）",
                        text_style(12.0, 400, Color::WHITE),
                    )
                    .style(StyleBuilder::new().padding(px(8.0)).build())
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(theme.role(cx, Role::FgDefault)),
                        ..StylePatch::default()
                    })
                    .build(tree, panel)?;
                    Ok(())
                });
                if opened.is_ok() {
                    log_line(app, &log, "overlay: 打开（锚点=触发节点）");
                }
            }
        });

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
                let opened = menu.open(app, window, Anchor::node(node));
                if opened.is_ok() {
                    log_line(app, &log, "menu: 打开（↑↓ / Enter / Esc / 子菜单 →）");
                }
            }
        });

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
                    .and_then(|target| target.tree().get(node).ok())
                    .map(|node| {
                        let bounds = node.layout().bounds;
                        LogicalPoint::new(
                            bounds.origin.x + 24.0,
                            bounds.origin.y + bounds.size.height + 6.0,
                        )
                    })
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
            overlay_check: Rc::new(RefCell::new(OverlayCheckReport::default())),
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
        if self.flags.dump_overlay {
            let viewport = self.viewport;
            let menu = self.menu.clone();
            let color_picker = self.color_picker.clone();
            let nodes = self.probe_nodes.clone();
            let report = self.overlay_check.clone();
            let _ = app.after(window, Duration::from_millis(600), move |app| {
                if let Some(handle) = app.scroll_handle(window, viewport) {
                    let _ = app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(3_150.0)));
                }
                print_probe_rects(app, window, &nodes, "overlay ");
                app.handle().wake();
                let menu = menu.clone();
                let color_picker = color_picker.clone();
                let report = report.clone();
                let _ = app.after(window, Duration::from_millis(300), move |app| {
                    if let Some((_, anchor)) = nodes.iter().find(|(name, _)| name == "menu") {
                        let opened = menu.open(app, window, Anchor::node(*anchor));
                        println!("gallery: overlay-probe menu.open -> {opened:?}");
                    }
                    app.handle().wake();
                    for round in 0..5_u32 {
                        let menu = menu.clone();
                        let color_picker = color_picker.clone();
                        let report = report.clone();
                        let delay = Duration::from_millis(u64::from(600 * (round + 1)));
                        let step = move |app: &mut App| {
                            let frame = app
                                .window(window)
                                .map(|target| target.frame_index().get())
                                .unwrap_or(0);
                            match round {
                                0 | 1 => {
                                    let metrics = menu_metrics(app, window);
                                    println!(
                                        "gallery: overlay-check menu round={round} frame={frame} metrics={metrics:?}"
                                    );
                                    record_menu_metrics(&report, metrics);
                                    dump_overlay_nodes(app, window, &["gallery.menu:panel"]);
                                    if round == 1 {
                                        let closed = menu.close(app);
                                        let opened = match color_picker.open(app, window) {
                                            Ok(opened) => opened,
                                            Err(error) => {
                                                println!(
                                                    "gallery: overlay-check color-picker.open FAIL {error}"
                                                );
                                                false
                                            }
                                        };
                                        println!(
                                            "gallery: overlay-check menu.close -> {closed}; color-picker.open -> {opened}"
                                        );
                                    }
                                }
                                2 | 3 => {
                                    let (is_open, children) =
                                        picker_metrics(app, window, &color_picker);
                                    println!(
                                        "gallery: overlay-check picker round={round} frame={frame} is_open={is_open} content_children={children:?}"
                                    );
                                    record_picker_metrics(&report, is_open, children);
                                    dump_overlay_nodes(app, window, &["gallery.color-picker"]);
                                }
                                _ => {
                                    let (is_open, children) =
                                        picker_metrics(app, window, &color_picker);
                                    record_picker_metrics(&report, is_open, children);
                                    let passed = report.borrow().pass();
                                    println!(
                                        "gallery: overlay-check final round={round} frame={frame} {}",
                                        if passed { "PASS" } else { "FAIL" }
                                    );
                                    if !passed {
                                        println!(
                                            "gallery: overlay-check detail {:?}",
                                            report.borrow()
                                        );
                                    }
                                    let _ = color_picker.close(app);
                                    dump_overlay_nodes(
                                        app,
                                        window,
                                        &["gallery.menu:panel", "gallery.color-picker"],
                                    );
                                }
                            }
                            app.handle().wake();
                        };
                        if let Err(error) = app.after(window, delay, move |app| step(app)) {
                            println!("gallery: overlay-probe timer failed: {error}");
                            break;
                        }
                    }
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
        {
            let _ = app.after(window, Duration::from_secs(seconds), move |app| {
                println!("gallery: auto-close after {seconds}s");
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
        let _ = self.context_menu.sync(app);
        let _ = self.overlay.sync(app);
        let _ = self.tooltip.sync(app);
        let _ = self.dialog.sync(app);
        let _ = self.alert_dialog.sync(app);
        let _ = self.color_picker.sync(app);
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

fn dump_overlay_nodes(app: &mut App, window: WindowId, prefixes: &[&str]) {
    let Some(target) = app.window(window) else {
        println!("gallery: overlay-dump window missing");
        return;
    };
    let tree = target.tree();
    let root = tree.root();
    let mut stack: Vec<(NodeId, usize)> = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        let Ok(entry) = tree.get(node) else {
            continue;
        };
        let id = tree.debug_id(node).unwrap_or("<no-id>");
        if prefixes.iter().any(|prefix| id.starts_with(prefix)) {
            let bounds = entry.layout().bounds;
            let children = entry.children().len();
            let scroll = app
                .scroll_handle(window, node)
                .and_then(|handle| app.scroll_state(&handle));
            let scroll = scroll.map_or_else(
                || SmolStr::new_static("-"),
                |state| {
                    format_smolstr!(
                        "offset={:.1} content={:.1}x{:.1} viewport={:.1}x{:.1}",
                        state.offset.y,
                        state.content.width,
                        state.content.height,
                        state.viewport.width,
                        state.viewport.height
                    )
                },
            );
            println!(
                "gallery: overlay-dump {indent}{id} rect=({:.1},{:.1} {:.1}x{:.1}) children={children} scroll={scroll}",
                bounds.origin.x,
                bounds.origin.y,
                bounds.size.width,
                bounds.size.height,
                indent = "  ".repeat(depth)
            );
        }
        for child in entry.children().iter().rev() {
            stack.push((*child, depth + 1));
        }
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

fn record_menu_metrics(
    report: &Rc<RefCell<OverlayCheckReport>>,
    metrics: Option<(usize, f32, f32)>,
) {
    let Some(metrics) = metrics else {
        return;
    };
    let mut report = report.borrow_mut();
    report.menu = Some(match report.menu {
        Some((children, content, thumb)) => (
            children.max(metrics.0),
            content.max(metrics.1),
            thumb.max(metrics.2),
        ),
        None => metrics,
    });
}

fn record_picker_metrics(
    report: &Rc<RefCell<OverlayCheckReport>>,
    is_open: bool,
    children: Option<usize>,
) {
    let mut report = report.borrow_mut();
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
            && picker.overlay().revealed(app.app())
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
    let node = picker.overlay().panel_node()?;
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
        if picker.overlay().overlay_node().is_none() {
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
    let layer = Layer::new();
    let router = AnimationRouter::new();
    let picker = ColorPicker::new(
        app.app_mut(),
        theme,
        layer,
        router,
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
         real-window pixel readback/screenshot is P3 (--dump-overlay only asserts rects)"
    );
    println!(
        "gallery: pixel-check migration note: this gate binds kit::overlay/ColorPicker APIs that \
         P1-11 deletes; P1-12 must port it (registered in design §8 §11)"
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
    let settled = pixel_check_settle(&mut app, &picker, &mut pixels, &mut run, "open", true);
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
    pixel_check_close(&mut app, &picker, &mut pixels, &mut run, "close");
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
    pixel_check_close(&mut app, &picker, &mut pixels, &mut run, "reopen-close");
    run.assert_restored("reopen-close");

    let report = run.report;
    let passed = report.failures == 0;
    println!(
        "gallery: pixel-check checks={} sampled_frames={} damage_full_frames={} first_visible={:?} stable={:?} moved={:?}",
        report.checks,
        report.sampled_frames,
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
    if gallery.flags.dump_overlay {
        let report = gallery.overlay_check.borrow();
        if report.pass() {
            println!(
                "gallery: overlay-check PASS (menu={:?} picker_open={} picker_children={:?})",
                report.menu, report.picker_open, report.picker_panel_children
            );
        } else {
            println!("gallery: overlay-check FAIL ({report:?})");
            failed = true;
        }
    }
    if failed {
        std::process::exit(3);
    }
    Ok(())
}

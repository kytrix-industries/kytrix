use std::time::Duration;

use smol_str::SmolStr;
use ui_core::{
    a11y::A11yRole,
    app::{App, Application},
    math::{LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        color::Color,
        style::Style,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::TextStyle,
    text_edit::{EditableSpec, TextEditMetrics, TextEditSnapshot},
    window::{WindowId, WindowOptions},
};

const TITLE: &str = "Kytrix · editable text engine";

#[derive(Clone, Copy)]
struct Scene {
    single: NodeId,
    multiline: NodeId,
}

fn spec(multiline: bool, label: &'static str, placeholder: &'static str) -> EditableSpec {
    EditableSpec {
        role: if multiline {
            A11yRole::MultilineTextInput
        } else {
            A11yRole::TextInput
        },
        label: SmolStr::new_static(label),
        placeholder: SmolStr::new_static(placeholder),
        multiline,
        masked: false,
        readonly: false,
        disabled: false,
        max_length: None,
        submit: None,
        text_style: TextStyle::new(px(16.0)),
        caret_color: Color::from_rgba8([120, 200, 255, 255]),
        selection_color: Color::from_rgba8([60, 120, 200, 110]),
        placeholder_color: Color::from_rgba8([120, 128, 140, 255]),
        composition_color: Color::from_rgba8([120, 200, 255, 255]),
    }
}

fn editable_style(width: f32) -> Style {
    let mut style = Style::default();
    style.align_self(taffy::style::AlignSelf::START);
    style.flex(0.0, 0.0, taffy::style::Dimension::auto());
    style.width(taffy::style::Dimension::length(width));
    style
}

fn build_scene(app: &mut App, window: WindowId) -> Scene {
    let root = app.window(window).expect("window exists").tree().root();
    let panel = {
        let target = app.window_mut(window).expect("window exists");
        NodeBuilder::boxed()
            .id("editable-demo")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .padding(px(28.0))
                    .gap(px(14.0))
                    .build(),
            )
            .paint(
                PaintBuilder::new()
                    .background(Color::from_rgba8([22, 26, 34, 255]))
                    .build(),
            )
            .build(target.tree_mut(), root)
            .expect("panel")
    };
    let target = app.window_mut(window).expect("window exists");
    let tree = target.tree_mut();
    let _title = NodeBuilder::text("Engine-level editable text", TextStyle::new(px(22.0)))
        .build(tree, panel)
        .expect("title");
    let _hint = NodeBuilder::text(
        "Click, drag, double-click and type — no ui_kit involved.",
        TextStyle::new(px(13.0)),
    )
    .build(tree, panel)
    .expect("hint");
    let single = NodeBuilder::editable_text(spec(false, "Single line", "Type here"))
        .id("editable-single")
        .style(editable_style(360.0))
        .build(tree, panel)
        .expect("single line");
    let _spacer = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_none()
                .size(px(1.0), px(10.0))
                .build(),
        )
        .build(tree, panel)
        .expect("spacer");
    let _multiline_label = NodeBuilder::text(
        "Multiline (Enter inserts a newline)",
        TextStyle::new(px(13.0)),
    )
    .build(tree, panel)
    .expect("label");
    let multiline =
        NodeBuilder::editable_text(spec(true, "Multiline", "Drag to select across lines"))
            .id("editable-multiline")
            .style(editable_style(420.0))
            .build(tree, panel)
            .expect("multiline");
    let _footer = NodeBuilder::text(
        "engine: selection / caret / placeholder / IME are owned by ui_core",
        TextStyle::new(px(12.0)),
    )
    .build(tree, panel)
    .expect("footer");
    app.text_edit_set_value(
        window,
        single,
        "Hello editable text — drag across this line.",
    )
    .expect("single value");
    app.text_edit_set_value(
        window,
        multiline,
        "engine owned selection\nsecond line\nthird line",
    )
    .expect("multiline value");
    Scene { single, multiline }
}

fn node_rect(app: &App, window: WindowId, node: NodeId) -> Option<LogicalRect> {
    app.window(window)?
        .tree()
        .get(node)
        .ok()
        .map(|target| target.layout().bounds)
}

#[derive(Clone, PartialEq)]
struct ProbeEntry {
    name: &'static str,
    bounds: Option<LogicalRect>,
    value: Option<SmolStr>,
    anchor: usize,
    focus: usize,
    composing: bool,
    focused: bool,
    caret: Option<LogicalRect>,
    metrics: Option<TextEditMetrics>,
}

fn probe_entries(app: &App, window: WindowId, scene: &Scene) -> [ProbeEntry; 2] {
    let mut entries = [
        ProbeEntry {
            name: "single",
            bounds: None,
            value: None,
            anchor: 0,
            focus: 0,
            composing: false,
            focused: false,
            caret: None,
            metrics: None,
        },
        ProbeEntry {
            name: "multiline",
            bounds: None,
            value: None,
            anchor: 0,
            focus: 0,
            composing: false,
            focused: false,
            caret: None,
            metrics: None,
        },
    ];
    for (entry, node) in entries.iter_mut().zip([scene.single, scene.multiline]) {
        entry.bounds = node_rect(app, window, node);
        entry.value = app.text_edit_value(window, node).ok();
        entry.metrics = app.text_edit_metrics(window, node).ok();
        let Ok(snapshot): Result<TextEditSnapshot, _> = app.text_edit_snapshot(window, node) else {
            continue;
        };
        entry.anchor = snapshot.selection.anchor().byte;
        entry.focus = snapshot.selection.focus().byte;
        entry.composing = snapshot.composing;
        entry.focused = snapshot.focused;
        entry.caret = app
            .window(window)
            .and_then(|target| target.text_caret_rect(node, entry.focus).ok());
    }
    entries
}

fn print_probe(label: &str, entries: &[ProbeEntry; 2]) {
    for entry in entries {
        println!(
            "probe[{label}] {}: bounds={:?} caret={:?} anchor={} focus={} composing={} focused={} metrics={:?} value={:?}",
            entry.name,
            entry.bounds,
            entry.caret,
            entry.anchor,
            entry.focus,
            entry.composing,
            entry.focused,
            entry.metrics,
            entry.value
        );
    }
}

fn run_headless(probe: bool) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: SmolStr::new_static(TITLE),
        size: LogicalSize::new(px(720.0), px(480.0)),
        background: Color::from_rgba8([18, 21, 27, 255]),
        ..WindowOptions::default()
    });
    let scene = build_scene(&mut app, window);
    for _ in 0..4 {
        let outcome = app
            .frame(window)
            .expect("window exists")
            .expect("headless frame");
        let _ = outcome;
    }
    let single_bounds = node_rect(&app, window, scene.single).expect("single bounds");
    let multiline_bounds = node_rect(&app, window, scene.multiline).expect("multiline bounds");
    assert!(
        single_bounds.size.height > px(0.0) && multiline_bounds.size.height > px(0.0),
        "editable nodes must be measured"
    );
    app.text_edit_metrics(window, scene.single)
        .expect("metrics");
    let single_value = app.text_edit_value(window, scene.single).expect("value");
    let single_snapshot = app
        .text_edit_snapshot(window, scene.single)
        .expect("single snapshot");
    assert!(
        single_snapshot.selection.is_collapsed(),
        "headless single line must start collapsed"
    );
    assert_eq!(
        single_snapshot.selection.focus().byte,
        single_value.len(),
        "headless single line selection sits at the end of the value"
    );
    assert!(!single_snapshot.focused);
    let multiline_value = app
        .text_edit_value(window, scene.multiline)
        .expect("multiline value");
    let multiline_snapshot = app
        .text_edit_snapshot(window, scene.multiline)
        .expect("multiline snapshot");
    assert_eq!(
        multiline_snapshot.selection.range(),
        multiline_value.len()..multiline_value.len(),
        "headless multiline selection sits at the end of the value"
    );
    println!(
        "headless: single={single_bounds:?} multiline={multiline_bounds:?} value={single_value:?} selection={:?}",
        multiline_snapshot.selection.range()
    );
    if probe {
        print_probe("headless", &probe_entries(&app, window, &scene));
    }
    println!("headless: OK");
    Ok(())
}

fn auto_close_after(args: &[String]) -> Option<Duration> {
    let index = args.iter().position(|arg| arg == "--auto-close")?;
    let seconds = args
        .get(index + 1)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(5.0);
    Some(Duration::from_secs_f32(seconds))
}

fn run_windowed(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let probe = args.iter().any(|arg| arg == "--probe");
    let auto_close = auto_close_after(args);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: SmolStr::new_static(TITLE),
        size: LogicalSize::new(px(720.0), px(480.0)),
        background: Color::from_rgba8([18, 21, 27, 255]),
        ..WindowOptions::default()
    });
    let scene = build_scene(&mut app, window);
    if let Some(limit) = auto_close {
        println!("windowed: auto-close after {:.1}s", limit.as_secs_f32());
        app.after(window, limit, move |app| {
            println!(
                "windowed: closing after {:.2}s (selection={:?})",
                limit.as_secs_f32(),
                app.text_edit_snapshot(window, scene.multiline)
                    .ok()
                    .map(|snapshot| snapshot.selection.range())
            );
            app.close_window(window);
            app.handle().wake();
        })?;
    }
    let mut last_probe: Option<[ProbeEntry; 2]> = None;
    let mut focused_initial = false;
    app.on_frame(move |app| {
        if app.window(window).is_none() {
            return;
        }
        if !focused_initial {
            focused_initial = true;
            app.set_focus(window, Some(scene.single));
        }
        if probe {
            let entries = probe_entries(app, window, &scene);
            if last_probe.as_ref() != Some(&entries) {
                print_probe("windowed", &entries);
                last_probe = Some(entries);
            }
        }
    });
    Application::new().run(app)?;
    println!("windowed: event loop exited");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--headless") {
        return run_headless(args.iter().any(|arg| arg == "--probe"));
    }
    run_windowed(&args)
}

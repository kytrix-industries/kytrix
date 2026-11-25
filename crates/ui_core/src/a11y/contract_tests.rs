use accesskit::{Action, NodeId as AkNodeId, TreeId};
use accesskit_consumer::{NodeRef, Tree, TreeChangeHandler, TreeState};
use smol_str::SmolStr;

use crate::{
    a11y::{
        A11yNodeId, A11yRole, A11yTreeSnapshot,
        adapter::{to_accesskit, to_tree_update},
    },
    app::App,
    event::{Event, ImeEvent},
    math::px,
    node::{builder::NodeBuilder, id::NodeId},
    paint::color::Color,
    text::TextStyle,
    text_edit::EditableSpec,
    window::{WindowId, WindowOptions},
};

struct NoopChanges;

impl TreeChangeHandler for NoopChanges {
    fn node_added(&mut self, _node: &NodeRef) {}

    fn node_updated(&mut self, _old_node: &NodeRef, _new_node: &NodeRef) {}

    fn focus_moved(&mut self, _old_node: Option<&NodeRef>, _new_node: Option<&NodeRef>) {}

    fn node_removed(&mut self, _node: &NodeRef) {}
}

fn new_app() -> (tokio::runtime::Runtime, App, WindowId) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions::default());
    (runtime, app, window)
}

fn editable_spec(role: A11yRole, multiline: bool, masked: bool) -> EditableSpec {
    EditableSpec {
        role,
        label: SmolStr::new_static("输入"),
        placeholder: SmolStr::new_static(""),
        multiline,
        masked,
        readonly: false,
        disabled: false,
        max_length: None,
        submit: None,
        text_style: TextStyle::new(px(16.0)),
        caret_color: Color::BLACK,
        selection_color: Color::BLACK,
        placeholder_color: Color::BLACK,
        composition_color: Color::BLACK,
    }
}

fn mount(app: &mut App, window: WindowId, editable: EditableSpec, text: &str) -> NodeId {
    let target = app.window_mut(window).expect("window");
    let root = target.tree().root();
    let node = NodeBuilder::editable_text(editable)
        .build(target.tree_mut(), root)
        .expect("mount editable text");
    if !text.is_empty() {
        assert!(
            app.text_edit_set_value(window, node, text)
                .expect("set value"),
            "非空初值必须报告变化"
        );
    }
    node
}

fn frame(app: &mut App, window: WindowId) {
    app.frame(window).expect("window").expect("frame");
}

fn full_tree(app: &mut App, window: WindowId) -> (A11yTreeSnapshot, Tree) {
    frame(app, window);
    let snapshot = app.a11y_take(window).expect("first a11y delivery");
    assert!(snapshot.full, "首笔必须全树");
    let tree = Tree::new(to_tree_update(&snapshot), true);
    (snapshot, tree)
}

fn incremental(app: &mut App, window: WindowId, tree: &mut Tree) -> A11yTreeSnapshot {
    frame(app, window);
    let snapshot = app.a11y_take(window).expect("incremental a11y delivery");
    tree.update_and_process_changes(to_tree_update(&snapshot), &mut NoopChanges);
    snapshot
}

fn node_ref<'a>(state: &'a TreeState, id: A11yNodeId) -> NodeRef<'a> {
    state
        .node_by_tree_local_id(AkNodeId(id.get()), TreeId::ROOT)
        .expect("a11y node must be in the consumer tree")
}

fn text_runs(snapshot: &A11yTreeSnapshot, input: NodeId) -> (A11yNodeId, A11yNodeId) {
    let record = snapshot.node_for(input).expect("input record");
    let run = record
        .text_run
        .expect("editable input must own a TextRun child");
    assert_eq!(
        record.children.first().copied(),
        Some(run),
        "输入节点的 children 首位必须是它的 TextRun 子节点"
    );
    (record.id, run)
}

#[test]
fn an_editable_input_exposes_a_resolvable_text_run_selection() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::TextInput, false, false),
        "价🌍ab",
    );
    assert!(crate::text_edit::set_selection(&mut app, window, input, 3, 8).expect("select 🌍a"));
    let (snapshot, tree) = full_tree(&mut app, window);
    let (input_id, run_id) = text_runs(&snapshot, input);
    let state = tree.state();
    let input_node = node_ref(state, input_id);
    let run_node = node_ref(state, run_id);

    assert!(
        input_node.supports_text_ranges(),
        "输入节点 + TextRun 子节点必须让 consumer 认为支持文本范围"
    );
    assert_eq!(run_node.role(), accesskit::Role::TextRun);
    assert_eq!(run_node.parent().expect("run parent").id(), input_node.id());
    assert_eq!(run_node.data().value(), Some("价🌍ab"));
    assert_eq!(run_node.data().character_lengths(), &[3, 4, 1, 1]);
    let word_starts = run_node.data().word_starts();
    assert_eq!(word_starts.first().copied(), Some(0));
    assert!(word_starts.windows(2).all(|pair| pair[0] < pair[1]));

    let selection = input_node
        .text_selection()
        .expect("TextSelection 必须可解析（TextPosition.node = TextRun 子节点）");
    assert_eq!(
        selection.start().inner_node().id(),
        run_node.id(),
        "anchor 必须指向 TextRun 子节点"
    );
    assert_eq!(
        selection.end().inner_node().id(),
        run_node.id(),
        "focus 必须指向 TextRun 子节点"
    );
    assert_ne!(
        selection.start().inner_node().id(),
        input_node.id(),
        "不得再自引用输入节点（consumer 会 panic）"
    );
    assert_eq!(
        (
            selection.start().to_global_usv_index(),
            selection.end().to_global_usv_index()
        ),
        (1, 3),
        "character_index 必须按 grapheme（费 1 / 🌍 1 / a 1）"
    );
    assert_eq!(selection.text(), "🌍a");
    assert_eq!(input_node.document_range().text(), "价🌍ab");
    assert_eq!(input_node.document_start().inner_node().id(), run_node.id());
    assert_eq!(
        input_node.document_end().to_global_usv_index(),
        "价🌍ab".chars().count()
    );
}

#[test]
fn an_empty_editable_input_still_supports_text_ranges_and_a_caret() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::TextInput, false, false),
        "",
    );
    let (snapshot, tree) = full_tree(&mut app, window);
    let (input_id, run_id) = text_runs(&snapshot, input);
    let state = tree.state();
    let input_node = node_ref(state, input_id);
    let run_node = node_ref(state, run_id);

    assert!(input_node.supports_text_ranges());
    assert_eq!(run_node.data().value(), Some(""));
    assert!(run_node.data().character_lengths().is_empty());
    let selection = input_node.text_selection().expect("空文本也要交付 caret");
    assert_eq!(
        (
            selection.start().to_global_usv_index(),
            selection.end().to_global_usv_index()
        ),
        (0, 0)
    );
    assert_eq!(selection.text(), "");
}

#[test]
fn a_password_input_only_exposes_the_masked_text_run() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::PasswordInput, false, true),
        "s3cret",
    );
    assert!(crate::text_edit::set_selection(&mut app, window, input, 3, 6).expect("select"));
    let (snapshot, tree) = full_tree(&mut app, window);
    let (input_id, run_id) = text_runs(&snapshot, input);
    let state = tree.state();
    let input_node = node_ref(state, input_id);
    let run_node = node_ref(state, run_id);

    assert_eq!(input_node.role(), accesskit::Role::PasswordInput);
    assert_eq!(input_node.data().value(), Some("••••••"));
    assert_eq!(run_node.data().value(), Some("••••••"));
    assert_eq!(run_node.data().character_lengths(), &[3, 3, 3, 3, 3, 3]);
    assert_eq!(input_node.document_range().text(), "••••••");
    assert_eq!(
        node_ref(state, input_id)
            .text_selection()
            .expect("selection")
            .text(),
        "•"
    );
    let exposed = format!(
        "{:?}{:?}{:?}",
        input_node.data().value(),
        run_node.data().value(),
        input_node.document_range().text()
    );
    assert!(
        !exposed.contains("s3cret"),
        "a11y 树不得泄漏密码明文：{exposed}"
    );
    assert_no_plaintext(&snapshot, "s3cret");
}

#[test]
fn a_composing_password_input_keeps_the_a11y_value_masked() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::PasswordInput, false, true),
        "s3cret",
    );
    let (snapshot, mut tree) = full_tree(&mut app, window);
    let _ = text_runs(&snapshot, input);

    assert!(app.set_focus(window, Some(input)));
    app.dispatch(
        window,
        Event::Ime(ImeEvent::Preedit {
            text: SmolStr::new_static("mima"),
            caret: Some(4),
            clauses: smallvec::SmallVec::new(),
            replace: None,
        }),
    );
    let composition = app
        .text_edit_composition(window, input)
        .expect("组合状态")
        .expect("组合中");
    assert!(!composition.preedit.is_empty(), "前置：已进入组合");

    let snapshot = incremental(&mut app, window, &mut tree);
    let (input_id, _run_id) = text_runs(&snapshot, input);
    let state = tree.state();
    let input_node = node_ref(state, input_id);

    assert_eq!(
        input_node.data().value(),
        Some("••••••"),
        "组合期的 a11y value 仍是已提交文本的掩码串"
    );
    assert_no_plaintext(&snapshot, "s3cret");
    assert_no_plaintext(&snapshot, "mima");
}

fn assert_no_plaintext(snapshot: &A11yTreeSnapshot, secret: &str) {
    let mut records = Vec::with_capacity(snapshot.nodes().len() + 2);
    records.extend(snapshot.nodes().iter());
    records.push(snapshot.focus_node());
    records.extend(snapshot.focus_ancestors.iter());
    if let Some(run) = &snapshot.focus_text_run {
        records.push(run);
    }
    for record in records {
        for text in [
            record.value.as_deref(),
            record.label_text(),
            record.description.as_deref(),
            record.placeholder.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(!text.contains(secret), "a11y 记录泄漏密码明文：{text}");
        }
    }
    let update = to_tree_update(snapshot);
    for (id, node) in &update.nodes {
        for text in [
            node.value(),
            node.label(),
            node.description(),
            node.placeholder(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(
                !text.contains(secret),
                "TreeUpdate 节点 {id:?} 泄漏密码明文：{text}"
            );
        }
    }
}

#[test]
fn a_reversed_selection_resolves_without_panicking() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::TextInput, false, false),
        "价🌍ab",
    );
    assert!(crate::text_edit::set_selection(&mut app, window, input, 8, 0).expect("反向选区"));
    let (snapshot, tree) = full_tree(&mut app, window);
    let (input_id, _) = text_runs(&snapshot, input);
    let input_node = node_ref(tree.state(), input_id);
    let selection = input_node.text_selection().expect("反向选区必须可解析");
    assert_eq!(selection.text(), "价🌍a", "consumer 按文档顺序归一化");
    assert_eq!(
        (
            selection.start().to_global_usv_index(),
            selection.end().to_global_usv_index()
        ),
        (0, 3)
    );
}

#[test]
fn text_selection_and_replace_actions_are_advertised() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::TextInput, false, false),
        "abc",
    );
    let (snapshot, tree) = full_tree(&mut app, window);
    let (input_id, run_id) = text_runs(&snapshot, input);
    let state = tree.state();
    let input_node = node_ref(state, input_id);
    let run_node = node_ref(state, run_id);

    assert!(
        !input_node.data().supports_action(Action::SetValue),
        "引擎不处理 SetValue：可编辑节点不得向 AT 广告该动作"
    );
    assert!(input_node.data().supports_action(Action::SetTextSelection));
    assert!(
        input_node
            .data()
            .supports_action(Action::ReplaceSelectedText)
    );
    assert!(run_node.data().supports_action(Action::SetTextSelection));

    let record = snapshot.node_for(input).expect("input record");
    let converted = to_accesskit(record);
    assert!(converted.supports_action(Action::SetTextSelection));
    assert!(converted.supports_action(Action::ReplaceSelectedText));
    assert_eq!(
        converted.text_selection().expect("selection").focus.node,
        AkNodeId(run_id.get())
    );
}

#[test]
fn incremental_updates_keep_the_text_run_and_selection_resolvable() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::TextInput, false, false),
        "ab",
    );
    assert!(crate::text_edit::set_selection(&mut app, window, input, 0, 1).expect("select a"));
    let (snapshot, mut tree) = full_tree(&mut app, window);
    let (input_id, _run_id) = text_runs(&snapshot, input);
    {
        let input_node = node_ref(tree.state(), input_id);
        assert_eq!(input_node.text_selection().expect("selection").text(), "a");
    }

    assert!(crate::text_edit::set_selection(&mut app, window, input, 0, 3).expect("select ab"));
    let _ = incremental(&mut app, window, &mut tree);
    {
        let input_node = node_ref(tree.state(), input_id);
        assert_eq!(input_node.text_selection().expect("selection").text(), "ab");
    }

    assert!(
        app.text_edit_set_value(window, input, "abc")
            .expect("set value"),
        "写入必须报告变化"
    );
    let snapshot = incremental(&mut app, window, &mut tree);
    let (input_id, run_id) = text_runs(&snapshot, input);
    let state = tree.state();
    let input_node = node_ref(state, input_id);
    let run_node = node_ref(state, run_id);
    assert_eq!(run_node.data().value(), Some("abc"));
    assert_eq!(run_node.data().character_lengths(), &[1, 1, 1]);
    let selection = input_node
        .text_selection()
        .expect("selection stays resolvable");
    assert_eq!(
        (
            selection.start().to_global_usv_index(),
            selection.end().to_global_usv_index()
        ),
        (3, 3)
    );
}

#[test]
fn composition_preedit_is_not_exposed_through_text_ranges() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::TextInput, false, false),
        "abcd",
    );
    assert!(crate::text_edit::set_selection(&mut app, window, input, 2, 2).expect("caret"));
    let (snapshot, mut tree) = full_tree(&mut app, window);
    let _ = text_runs(&snapshot, input);

    assert!(app.set_focus(window, Some(input)));
    app.dispatch(
        window,
        Event::Ime(ImeEvent::Preedit {
            text: SmolStr::new_static("ni"),
            caret: Some(2),
            clauses: smallvec::SmallVec::new(),
            replace: None,
        }),
    );
    let snapshot = incremental(&mut app, window, &mut tree);
    let (input_id, run_id) = text_runs(&snapshot, input);
    {
        let state = tree.state();
        let input_node = node_ref(state, input_id);
        assert!(
            input_node.text_selection().is_none(),
            "组合期不暴露 preedit：选区一并不暴露"
        );
        assert_eq!(input_node.document_range().text(), "abcd", "不含 preedit");
        assert_eq!(node_ref(state, run_id).data().value(), Some("abcd"));
    }

    app.dispatch(
        window,
        Event::Ime(ImeEvent::Commit {
            text: SmolStr::new_static("你"),
            replace: None,
        }),
    );
    let snapshot = incremental(&mut app, window, &mut tree);
    let (input_id, run_id) = text_runs(&snapshot, input);
    let state = tree.state();
    assert_eq!(node_ref(state, run_id).data().value(), Some("ab你cd"));
    let current = node_ref(state, input_id);
    let selection = current.text_selection().expect("提交后恢复选区");
    assert_eq!(
        (
            selection.start().to_global_usv_index(),
            selection.end().to_global_usv_index()
        ),
        (3, 3),
        "组合提交后 caret 跟在插入文本之后（grapheme 3）"
    );
    assert_eq!(selection.text(), "");
}

#[test]
fn a_multiline_input_keeps_newlines_inside_one_text_run() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::MultilineTextInput, true, false),
        "ab\ncd",
    );
    assert!(crate::text_edit::set_selection(&mut app, window, input, 1, 4).expect("跨行选区"));
    let (snapshot, tree) = full_tree(&mut app, window);
    let (input_id, run_id) = text_runs(&snapshot, input);
    let state = tree.state();
    let input_node = node_ref(state, input_id);
    let run_node = node_ref(state, run_id);

    assert!(input_node.supports_text_ranges());
    assert_eq!(run_node.data().value(), Some("ab\ncd"));
    assert_eq!(run_node.data().character_lengths(), &[1, 1, 1, 1, 1]);
    let selection = input_node.text_selection().expect("跨行选区");
    assert_eq!(selection.text(), "b\nc");
    assert_eq!(input_node.document_range().text(), "ab\ncd");
}

#[test]
fn text_run_identity_and_selection_survive_many_full_deliveries() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        editable_spec(A11yRole::TextInput, false, false),
        "价🌍ab",
    );
    assert!(crate::text_edit::set_selection(&mut app, window, input, 3, 8).expect("select"));
    let (snapshot, mut tree) = full_tree(&mut app, window);
    let (input_id, run_id) = text_runs(&snapshot, input);

    for _ in 0..200 {
        app.window_mut(window).expect("window").a11y_force_full();
        frame(&mut app, window);
        let forced = app.a11y_take(window).expect("强制全量交付必须产出");
        assert!(forced.full, "强制全量后的首笔必须是全树");
        tree.update_and_process_changes(to_tree_update(&forced), &mut NoopChanges);
        let (forced_input, forced_run) = text_runs(&forced, input);
        assert_eq!(forced_input, input_id, "输入 id 必须跨全量交付稳定");
        assert_eq!(forced_run, run_id, "虚拟 TextRun id 必须跨全量交付稳定");
    }

    let state = tree.state();
    let input_node = node_ref(state, input_id);
    let run_node = node_ref(state, run_id);
    assert_eq!(run_node.data().value(), Some("价🌍ab"));
    assert!(input_node.supports_text_ranges());
    let selection = input_node.text_selection().expect("200 笔后选区仍可解析");
    assert_eq!(selection.text(), "🌍a");
}

fn unlabelled_editable_spec(role: A11yRole, multiline: bool, masked: bool) -> EditableSpec {
    EditableSpec {
        label: SmolStr::default(),
        ..editable_spec(role, multiline, masked)
    }
}

#[test]
fn an_unlabelled_editable_input_exposes_no_accessible_name() {
    let (_runtime, mut app, window) = new_app();
    let input = mount(
        &mut app,
        window,
        unlabelled_editable_spec(A11yRole::TextInput, false, false),
        "secret-value",
    );
    let (snapshot, tree) = full_tree(&mut app, window);
    let (input_id, _run_id) = text_runs(&snapshot, input);
    let input_node = node_ref(tree.state(), input_id);

    assert_eq!(
        input_node.label(),
        None,
        "空 label 的可编辑节点不得把内容文本当 Name"
    );
    assert_eq!(
        input_node.value().as_deref(),
        Some("secret-value"),
        "内容仍必须通过 value 暴露"
    );
    assert_eq!(
        snapshot.node_for(input).expect("input record").label_text(),
        None,
        "A11yNode 读面同样不得回退到内容文本"
    );
}

fn accessible_names(
    snapshot: &A11yTreeSnapshot,
    tree: &Tree,
    input: NodeId,
) -> (Option<String>, Option<String>) {
    let record = snapshot.node_for(input).expect("input record");
    (
        record.label_text().map(str::to_owned),
        node_ref(tree.state(), record.id).label(),
    )
}

#[test]
fn composition_never_changes_the_accessible_name() {
    let (_runtime, mut app, window) = new_app();
    let unlabelled = mount(
        &mut app,
        window,
        unlabelled_editable_spec(A11yRole::TextInput, false, false),
        "ab",
    );
    let labelled = mount(
        &mut app,
        window,
        editable_spec(A11yRole::TextInput, false, false),
        "ab",
    );
    let (snapshot, mut tree) = full_tree(&mut app, window);
    let (unlabelled_record_name, unlabelled_consumer_name) =
        accessible_names(&snapshot, &tree, unlabelled);
    let (labelled_record_name, labelled_consumer_name) =
        accessible_names(&snapshot, &tree, labelled);
    assert_eq!(unlabelled_record_name, None);
    assert_eq!(
        unlabelled_consumer_name, None,
        "组合前：无 label 的输入没有 Name"
    );
    assert_eq!(labelled_record_name.as_deref(), Some("输入"));
    assert_eq!(labelled_consumer_name.as_deref(), Some("输入"));

    assert!(app.set_focus(window, Some(unlabelled)));
    app.dispatch(
        window,
        Event::Ime(ImeEvent::Preedit {
            text: SmolStr::new_static("ni"),
            caret: Some(2),
            clauses: smallvec::SmallVec::new(),
            replace: None,
        }),
    );
    assert!(
        app.text_edit_composition(window, unlabelled)
            .expect("composition")
            .is_some(),
        "前置：组合已开始"
    );
    let snapshot = incremental(&mut app, window, &mut tree);
    let record = snapshot.node_for(unlabelled).expect("input record");
    assert_eq!(record.label_text(), None, "组合期 name 不得随 preedit 变化");
    assert_eq!(
        node_ref(tree.state(), record.id).label(),
        None,
        "consumer 读面同样保持无 Name"
    );
    let labelled_record = snapshot.node_for(labelled).expect("labelled record");
    assert_eq!(labelled_record.label_text(), Some("输入"));
    assert_eq!(
        node_ref(tree.state(), labelled_record.id)
            .label()
            .as_deref(),
        Some("输入"),
        "有 label 时 name 在组合期保持恒定"
    );
}

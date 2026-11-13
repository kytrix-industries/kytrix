//! ⑤ 输入法：`ImeField` 单行模型 + 输入框（`design/phase-15-demo-showcase.md` §2.4）。
//!
//! 引擎只负责把平台 `WM_IME_*` / `NSTextInputClient` / `zwp_text_input` 翻译成
//! [`ImeEvent`](kytrix_gui::event::ImeEvent) 并交给应用；**组合串的语义**（提交后追加、组合中忽略
//! Backspace）是应用层的事，这里用值语义的 [`ImeField`] 表达，可脱离窗口单测。

use kytrix_gui::{
    a11y::{A11yAction, A11yOverride, A11yPatch, A11yRole},
    app::App,
    event::ImeEvent,
    math::Rect,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        primitive::Border,
        style::{Cursor, PaintSpec, StylePatch},
    },
    window::WindowId,
};
use smol_str::{SmolStr, format_smolstr};

use super::{
    DIM_RGBA, FIELD_BORDER_RGBA, FIELD_RGBA, MUTED_RGBA, TEXT_RGBA, card, color, ring, text_style,
};

/// 输入框的单行状态（`Signal<ImeField>` 的载荷）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ImeField {
    /// 已提交文本（真正"输入进去"的内容）。
    committed: SmolStr,
    /// 组合中的文本（`Preedit`）。系统可能只发 `Start` / `Commit` / `End` 而不发组合串
    /// （Windows 11 Microsoft Pinyin 实测如此），因此「是否在组合」不靠它判断，见 `composing`。
    preedit: SmolStr,
    /// `Start` 起、`Commit` / `End` / 失焦止的组合标记。
    composing: bool,
    /// 最近一条事件的可读形式。
    last_event: SmolStr,
    /// 最近一次 `Enter` 提交的内容。
    submitted: SmolStr,
    /// 累计收到的事件数（含 `Text` / Backspace / Enter）。
    events: u32,
}

impl ImeField {
    /// 离屏截图用的种子状态：已提交文本 + 组合串 + 一条最近提交。
    pub(crate) fn sample() -> Self {
        Self {
            committed: SmolStr::new_static("你好，Kytrix"),
            preedit: SmolStr::new_static("shi li"),
            composing: true,
            last_event: SmolStr::new_static("Preedit(\"shi li\")"),
            submitted: SmolStr::new_static("跨平台 GUI 引擎"),
            events: 12,
        }
    }

    /// 平台输入法事件（`Start` / `Preedit` / `Commit` / `End`）。
    pub(crate) fn apply(&mut self, event: &ImeEvent) {
        self.events = self.events.saturating_add(1);
        match event {
            ImeEvent::Start => {
                self.composing = true;
                self.last_event = SmolStr::new_static("Start");
            }
            ImeEvent::Preedit(text) => {
                self.composing = true;
                self.preedit = text.clone();
                self.last_event = format_smolstr!("Preedit(\"{text}\")");
            }
            ImeEvent::Commit(text) => {
                self.composing = false;
                self.committed = format_smolstr!("{}{text}", self.committed);
                self.preedit = SmolStr::default();
                self.last_event = format_smolstr!("Commit(\"{text}\")");
            }
            ImeEvent::End => {
                self.composing = false;
                self.preedit = SmolStr::default();
                self.last_event = SmolStr::new_static("End");
            }
        }
    }

    /// 非输入法的字符输入（`WM_CHAR` / 直接键入；IME 提交后平台已抑制重复字符）。
    pub(crate) fn insert(&mut self, text: &str) {
        self.events = self.events.saturating_add(1);
        self.committed = format_smolstr!("{}{text}", self.committed);
        self.last_event = format_smolstr!("Text(\"{text}\")");
    }

    /// 退格：组合中交给系统 IME（不改已提交文本），否则删最后一个 `char`。
    pub(crate) fn backspace(&mut self) {
        self.events = self.events.saturating_add(1);
        if self.composing() {
            self.last_event = SmolStr::new_static("Backspace（组合中，交给 IME）");
            return;
        }
        self.last_event = SmolStr::new_static("Backspace");
        let Some((index, _)) = self.committed.char_indices().next_back() else {
            return;
        };
        self.committed = SmolStr::new(&self.committed[..index]);
    }

    /// 回车：组合中忽略；否则把已提交文本记进「最近提交」并清空输入。
    pub(crate) fn submit(&mut self) {
        self.events = self.events.saturating_add(1);
        if self.composing() {
            self.last_event = SmolStr::new_static("Enter（组合中，交给 IME）");
            return;
        }
        self.last_event = SmolStr::new_static("Enter");
        if self.committed.is_empty() {
            return;
        }
        self.submitted = self.committed.clone();
        self.committed = SmolStr::default();
    }

    /// 无障碍 `SetValue`：AT 直接写入的文本替换整个输入内容（无选区模型，等价于全选替换）。
    pub(crate) fn set_text(&mut self, text: &str) {
        self.committed = SmolStr::new(text);
        self.preedit = SmolStr::default();
        self.composing = false;
        self.events = self.events.saturating_add(1);
        self.last_event = format_smolstr!("SetValue(\"{text}\")");
    }

    /// 焦点离开输入框 / 窗口失焦时收起组合态（否则 `Esc` 会被一直误判成「组合中」）。
    pub(crate) fn cancel_composition(&mut self) {
        if !self.composing {
            return;
        }
        self.composing = false;
        self.preedit = SmolStr::default();
        self.events = self.events.saturating_add(1);
        self.last_event = SmolStr::new_static("End（焦点离开输入框）");
    }

    /// 是否正在组合（`Start` / `Preedit` / `Commit` / `End` 驱动，不依赖系统是否发组合串）。
    pub(crate) fn composing(&self) -> bool {
        self.composing
    }

    /// 已提交文本（无障碍 `value` 与断言面）。
    pub(crate) fn committed(&self) -> &str {
        &self.committed
    }

    /// 输入框内显示的文本：已提交 + 组合串 + 焦点时的光标。
    pub(crate) fn display(&self, caret: bool) -> SmolStr {
        let text = format_smolstr!("{}{}", self.committed, self.preedit);
        if caret {
            format_smolstr!("{text}|")
        } else {
            text
        }
    }

    /// 「组合中：shi li」/「组合中：（输入法处理中）」/「组合中：（未开始）」。
    pub(crate) fn preedit_line(&self) -> SmolStr {
        if !self.composing {
            SmolStr::new_static("组合中：（未开始）")
        } else if self.preedit.is_empty() {
            // Windows 11 MSPY 只发 Start/Commit/End，组合串由 IME 自己的窗口显示。
            SmolStr::new_static("组合中：（输入法处理中）")
        } else {
            format_smolstr!("组合中：{}", self.preedit)
        }
    }

    /// 「最近 IME 事件：Preedit("shi li")（共 12 次）」。
    pub(crate) fn event_line(&self) -> SmolStr {
        let last = if self.last_event.is_empty() {
            "—"
        } else {
            self.last_event.as_str()
        };
        format_smolstr!("最近 IME 事件：{last}（共 {} 次）", self.events)
    }

    /// 「最近提交：跨平台 GUI 引擎」/「最近提交：（无）」。
    pub(crate) fn submitted_line(&self) -> SmolStr {
        if self.submitted.is_empty() {
            SmolStr::new_static("最近提交：（无）")
        } else {
            format_smolstr!("最近提交：{}", self.submitted)
        }
    }
}

/// 建输入框 + 三行状态 + 提示；返回输入框节点（可聚焦、`TextInput`）。
pub(crate) fn build(
    app: &mut App,
    window: WindowId,
    panel: NodeId,
    field: Signal<ImeField>,
    focused: Signal<bool>,
) -> NodeId {
    let field_node = NodeBuilder::boxed()
        .style(card::absolute_style(Rect::new(18.0, 40.0, 544.0, 54.0)))
        .paint(PaintSpec {
            background: color(FIELD_RGBA),
            border: Some(Border {
                width: 1.0,
                color: color(FIELD_BORDER_RGBA),
            }),
            corner_radius: [10.0; 4],
            ..PaintSpec::default()
        })
        .focus_ring(ring())
        .a11y(A11yOverride {
            role: Some(A11yRole::TextInput),
            label: Some(SmolStr::new_static("输入法测试输入框")),
            description: Some(SmolStr::new_static(
                "点击后用系统输入法输入中文；Enter 提交，Backspace 删除",
            )),
            // 引擎没有选区模型：只声明 `SetValue`（声明 `SetTextSelection` 而没有 handler 会被引擎
            // 记为 `UnhandledAction`，demo 不该留下这种噪音）。
            actions: Some(vec![A11yAction::SetValue]),
            ..A11yOverride::default()
        })
        .bind_style(|_cx| StylePatch {
            interactive: Some(true),
            cursor: Some(Cursor::Text),
            ..StylePatch::default()
        })
        .bind_a11y(move |cx| A11yPatch {
            value: Some(SmolStr::new(cx.read(&field).committed())),
            ..A11yPatch::default()
        })
        .build(card::tree(app, window), panel)
        .expect("输入框挂载");

    let _ = card::bound_hidden_label(
        app,
        window,
        field_node,
        Rect::new(14.0, 12.0, 516.0, 30.0),
        text_style(17.0, 400, TEXT_RGBA),
        move |cx| {
            let value = cx.read(&field);
            value.display(cx.read(&focused))
        },
    );
    let _ = card::bound_label(
        app,
        window,
        panel,
        Rect::new(18.0, 102.0, 544.0, 20.0),
        text_style(13.0, 400, MUTED_RGBA),
        move |cx| cx.read(&field).preedit_line(),
    );
    let _ = card::bound_label(
        app,
        window,
        panel,
        Rect::new(18.0, 126.0, 544.0, 18.0),
        text_style(11.5, 400, DIM_RGBA),
        move |cx| cx.read(&field).event_line(),
    );
    let _ = card::bound_label(
        app,
        window,
        panel,
        Rect::new(18.0, 148.0, 544.0, 18.0),
        text_style(11.5, 400, DIM_RGBA),
        move |cx| cx.read(&field).submitted_line(),
    );
    let _ = card::label(
        app,
        window,
        panel,
        Rect::new(18.0, 176.0, 544.0, 18.0),
        "点击输入框后用系统输入法输入拼音：组合串由系统 IME 窗口显示，上屏后追加到正文。",
        text_style(11.5, 400, DIM_RGBA),
    );
    let _ = card::label(
        app,
        window,
        panel,
        Rect::new(18.0, 198.0, 544.0, 18.0),
        "Enter 提交 / Backspace 删除 / Esc 退出（IME 组合由系统处理）。",
        text_style(11.0, 400, DIM_RGBA),
    );
    field_node
}

#[cfg(test)]
mod tests {
    use kytrix_gui::event::ImeEvent;

    use super::ImeField;

    fn typed(text: &str) -> ImeField {
        let mut field = ImeField::default();
        field.insert(text);
        field
    }

    #[test]
    fn composition_commits_into_the_text_and_clears_the_preedit() {
        let mut field = ImeField::default();

        field.apply(&ImeEvent::Start);
        assert!(field.composing(), "Start 本身就进入组合态（不依赖组合串）");
        field.apply(&ImeEvent::Preedit("ni".into()));
        assert!(field.composing());
        assert_eq!(field.display(false).as_str(), "ni");

        field.apply(&ImeEvent::Commit("你好".into()));
        assert!(!field.composing());
        assert_eq!(field.committed(), "你好");
        assert_eq!(field.display(true).as_str(), "你好|");
        assert_eq!(field.preedit_line().as_str(), "组合中：（未开始）");

        field.apply(&ImeEvent::End);
        assert!(!field.composing());
        assert_eq!(field.event_line().as_str(), "最近 IME 事件：End（共 4 次）");
    }

    /// Windows 11 的 Microsoft Pinyin 只发 Start / Commit / End：没有 `Preedit` 时
    /// `composing` 仍必须为真，否则组合中的 `Esc` 会被当成「退出应用」。
    #[test]
    fn start_alone_marks_the_composition_without_a_preedit_string() {
        let mut field = ImeField::default();

        field.apply(&ImeEvent::Start);

        assert!(field.composing());
        assert_eq!(field.display(false).as_str(), "");
        assert_eq!(
            field.preedit_line().as_str(),
            "组合中：（输入法处理中）",
            "系统不给组合串时也要能解释当前状态"
        );

        field.apply(&ImeEvent::End);
        assert!(!field.composing());
        assert_eq!(field.preedit_line().as_str(), "组合中：（未开始）");
    }

    #[test]
    fn preedit_is_replaced_not_appended() {
        let mut field = ImeField::default();

        field.apply(&ImeEvent::Preedit("s".into()));
        field.apply(&ImeEvent::Preedit("sh".into()));
        field.apply(&ImeEvent::Preedit("shi".into()));

        assert_eq!(field.committed(), "");
        assert_eq!(field.display(false).as_str(), "shi");
    }

    #[test]
    fn backspace_deletes_one_char_and_respects_the_composition() {
        let mut field = typed("ab你好");
        field.backspace();
        assert_eq!(field.committed(), "ab你", "按 char 而不是字节删除");

        field.apply(&ImeEvent::Preedit("ni".into()));
        field.backspace();
        assert_eq!(
            field.committed(),
            "ab你",
            "组合中的 Backspace 必须交给系统 IME"
        );
        assert!(
            field.event_line().as_str().contains("组合中"),
            "状态行要说明「交给 IME」：{}",
            field.event_line()
        );
    }

    #[test]
    fn backspace_on_empty_text_is_a_no_op() {
        let mut field = ImeField::default();

        field.backspace();

        assert_eq!(field.committed(), "");
        assert!(!field.composing());
    }

    #[test]
    fn enter_submits_and_clears_but_is_ignored_while_composing() {
        let mut field = typed("hello");

        field.apply(&ImeEvent::Preedit("ni".into()));
        field.submit();
        assert_eq!(field.committed(), "hello", "组合中的 Enter 交给 IME");

        field.apply(&ImeEvent::End);
        field.submit();
        assert_eq!(field.committed(), "");
        assert_eq!(field.submitted_line().as_str(), "最近提交：hello");
        assert_eq!(field.display(false).as_str(), "");
    }

    #[test]
    fn display_is_committed_plus_preedit_plus_optional_caret() {
        let mut field = typed("前缀");
        field.insert("后缀");

        assert_eq!(field.display(false).as_str(), "前缀后缀");
        assert_eq!(field.display(true).as_str(), "前缀后缀|");

        field.apply(&ImeEvent::Preedit("组".into()));
        assert_eq!(field.display(true).as_str(), "前缀后缀组|");
    }

    #[test]
    fn leaving_the_field_cancels_the_preedit_and_is_idempotent() {
        let mut field = ImeField::default();
        field.apply(&ImeEvent::Preedit("shi".into()));
        assert!(field.composing());

        field.cancel_composition();
        assert!(!field.composing(), "失焦必须收起组合串");
        assert_eq!(field.committed(), "", "已提交文本不受影响");
        assert_eq!(field.events, 2);

        field.cancel_composition();
        assert_eq!(field.events, 2, "没有组合态时不重复计数");
    }

    #[test]
    fn at_set_value_replaces_the_text_and_clears_the_preedit() {
        let mut field = ImeField::default();
        field.apply(&ImeEvent::Preedit("ni".into()));
        assert!(field.composing());

        field.set_text("无障碍输入");

        assert_eq!(field.committed(), "无障碍输入");
        assert!(!field.composing(), "SetValue 是整段替换，组合态必须结束");
        assert_eq!(
            field.event_line().as_str(),
            "最近 IME 事件：SetValue(\"无障碍输入\")（共 2 次）"
        );
    }

    #[test]
    fn sample_state_is_what_the_snapshot_renders() {
        let field = ImeField::sample();

        assert_eq!(field.committed(), "你好，Kytrix");
        assert!(field.composing());
        assert_eq!(field.display(false).as_str(), "你好，Kytrixshi li");
        assert_eq!(field.preedit_line().as_str(), "组合中：shi li");
        assert_eq!(field.submitted_line().as_str(), "最近提交：跨平台 GUI 引擎");
    }
}

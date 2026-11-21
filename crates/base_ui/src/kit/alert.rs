//! 对话框内容面板（旧 `crate::alert` 的引擎化重建，P2-4a）。
//!
//! 旧 `Alert` 是**浮层内容**：`LayerExt::alert` 把它挂在窗口层的 alert 槽里，遮罩 / 面板 / 进出场
//! 动画由 `dialog` 层负责；它自己只画「标题 + 消息 + 可选输入 + 按钮行」。本批（P2-4a）没有迁移
//! 浮层（`crate::layer` / `crate::dialog` 属 P2-4b），所以这里交付**同一块内容面板**，四种形状原样
//! 保留：
//!
//! | 构造器 | 形状 |
//! |---|---|
//! | [`AlertBuilder::ok`] | 标题 + 消息 + OK |
//! | [`AlertBuilder::ok_cancel`] | 标题 + 消息 + OK + Cancel |
//! | [`AlertBuilder::input_ok`] | 标题 + 消息 + 输入 + OK |
//! | [`AlertBuilder::input_ok_cancel`] | 标题 + 消息 + 输入 + OK + Cancel |
//!
//! `build` 返回 [`Alert`]，其中 `root` 是要挂进对话框面板的节点；P2-4b 的 `LayerExt::alert` 只是把它
//! 包进遮罩 + 面板 + 进出场动画，面板内容不需要重写。
//!
//! # 排版（按旧实现）
//!
//! 面板宽 384 px、列间距 8 px、裁剪子树；消息 16 px；输入框与错误行同列（间距 4 px），错误行
//! 14 px + `status.danger.base` 前景；按钮行由本模块自己布局 —— 单按钮铺满面板，双按钮各占一半
//! 且内容居中（旧 `Join + flex_1 + justify_center`；`Button` 不新增 flex 拉伸语义）。
//!
//! # 校验（旧语义原样保留）
//!
//! `validate` 在点 OK 时跑：`Err(message)` 把 message 写进错误行、**不**关闭（不调用 `ok_action`）；
//! `Ok(())` 清掉错误并调用 `ok_action(value)`（没有输入框的 alert 传 `None`）。旧实现用
//! `MarkdownState` 画消息与错误，新实现是纯文本（`kit::markdown` 的 view 是 P2-3 的独立组件，
//! 对话框内容的富文本接线随 P2-4b；登记见 spec-delta §3）。
//!
//! # 与 `kit::dialog` 组合（P2-4d1 / P2-4a3）
//!
//! alert 只是内容面板：容器 / 模态 / 焦点 / 遮罩 / 动画 / 对话框 a11y 角色都归
//! [`crate::kit::dialog`]（E12-R1：包装节点声明 `Dialog` / `AlertDialog`）。因此：
//!
//! - **内容根不声明对话框角色**，只是 `GenericContainer`（P2-4a3 修 P2-4d1 审查 D6：P2-4a
//!   交付时内容根声明 `Dialog`，与包装层重复，AT 会读两层对话框）；标题仍由标题文本节点提供；
//! - **关闭中守卫**：内容按钮（OK / Cancel）在容器开始关闭（淡出）后不再跑用户动作 —— 由
//!   dialog 挂载内容时经 `AlertBuilder::closing_guard` 注入。旧 dialog 在面板上
//!   `capture_any_mouse_down` 吞掉退场窗口内的按下；引擎化后内容节点仍是活节点，必须由容器
//!   告知（P2-4d1 审查 D2 的 alert 内容按钮同类缺陷）；
//! - **Esc / X 归 dialog**：alert 恒无关闭按钮（旧 alert 没有 X）；Esc 由引擎立即卸载浮层并报
//!   `Dismissed`，**不跑** `cancel_action`（引擎没有应用层关闭钩子，见 [`crate::kit::dialog`]
//!   模块文档「alert 的 Esc 语义」）；
//! - **按钮行**：铺满 / 等分 / 文字居中由 `kit::button` 的 `stretch_button` 统一实现，
//!   dialog 的确认 / 取消按钮行共用同一份（P2-4a3 去重，AGENTS §6）。
//!
//! # 与旧 API 的差异
//!
//! - 旧 builder 的输入是 `Entity<InputState>`；新 API 收一个**输入框工厂**（`FnOnce(&mut NodeTree,
//!   NodeId) -> InputNode`），因为新引擎的输入框是挂载期节点、必须由持有 `Theme` 的调用方建；
//! - 旧 alert 自己关闭（`window.dismiss_alert`）；新 alert 通过回调把关闭决定交给 P2-4b 的浮层；
//! - 旧 alert 没有关闭按钮、没有超时（确认：旧 `alert.rs` 全文无 `close` / timeout 分支），本批
//!   同样不提供 —— 关闭只有 OK / Cancel 两条路。

use std::{
    rc::Rc,
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};

use smol_str::SmolStr;
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    interaction::ActionCtx,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::style::StylePatch,
    text::{TextAlign, TextStyle},
    window::WindowId,
};

use crate::{
    kit::{
        button::{Button, stretch_button},
        input::InputNode,
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};

/// 面板宽度（旧 `.w_96()` = 24 rem = 384 px）。
const PANEL_WIDTH: f32 = 384.0;

/// alert 的四种形状。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertKind {
    /// 标题 + 消息 + OK。
    Ok,
    /// 标题 + 消息 + OK + Cancel。
    OkCancel,
    /// 标题 + 消息 + 输入 + OK。
    InputOk,
    /// 标题 + 消息 + 输入 + OK + Cancel。
    InputOkCancel,
}

impl AlertKind {
    /// 是否有 Cancel 按钮。
    #[must_use]
    pub fn with_cancel(&self) -> bool {
        matches!(self, AlertKind::OkCancel | AlertKind::InputOkCancel)
    }

    /// 是否有输入框。
    #[must_use]
    pub fn with_input(&self) -> bool {
        matches!(self, AlertKind::InputOk | AlertKind::InputOkCancel)
    }
}

/// 输入框工厂：调用方在 alert 的面板里挂一个 [`InputNode`]（持有 `Theme` 与状态信号）。
pub type InputFactory = Rc<dyn Fn(&mut NodeTree, NodeId) -> InputNode>;

/// 校验函数：`Err(message)` 显示在输入框下并阻止关闭。
pub type Validator = Rc<dyn Fn(&str) -> Result<(), SmolStr>>;

/// OK 动作：参数是输入值（没有输入框时为 `None`）。
pub type OkAction = Rc<dyn Fn(Option<SmolStr>, &mut App)>;

/// Cancel 动作。
pub type CancelAction = Rc<dyn Fn(&mut App)>;

static ALERT_SEQUENCE: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));

/// alert 面板的 builder（形状 / 文案 / 动作都选好之后 `build`）。
pub struct AlertBuilder {
    kind: AlertKind,
    title: SmolStr,
    message: SmolStr,
    input: Option<InputFactory>,
    ok_text: SmolStr,
    cancel_text: SmolStr,
    validate: Option<Validator>,
    ok_action: Option<OkAction>,
    cancel_action: Option<CancelAction>,
    /// 容器「关闭中」探针（由 [`crate::kit::dialog`] 注入，见 [`AlertBuilder::closing_guard`]）。
    closing: Option<Rc<dyn Fn() -> bool>>,
}

impl AlertBuilder {
    /// 空 builder（默认 `Ok` 形状、`OK` / `Cancel` 文案）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            kind: AlertKind::Ok,
            title: SmolStr::default(),
            message: SmolStr::default(),
            input: None,
            ok_text: SmolStr::new_static("OK"),
            cancel_text: SmolStr::new_static("Cancel"),
            validate: None,
            ok_action: None,
            cancel_action: None,
            closing: None,
        }
    }

    /// 标题 + 消息 + OK。
    #[must_use]
    pub fn ok(mut self, title: impl Into<SmolStr>, message: impl Into<SmolStr>) -> Self {
        self.kind = AlertKind::Ok;
        self.title = title.into();
        self.message = message.into();
        self
    }

    /// 标题 + 消息 + OK + Cancel。
    #[must_use]
    pub fn ok_cancel(mut self, title: impl Into<SmolStr>, message: impl Into<SmolStr>) -> Self {
        self.kind = AlertKind::OkCancel;
        self.title = title.into();
        self.message = message.into();
        self
    }

    /// 标题 + 消息 + 输入 + OK。
    #[must_use]
    pub fn input_ok(
        mut self,
        title: impl Into<SmolStr>,
        message: impl Into<SmolStr>,
        input: InputFactory,
    ) -> Self {
        self.kind = AlertKind::InputOk;
        self.title = title.into();
        self.message = message.into();
        self.input = Some(input);
        self
    }

    /// 标题 + 消息 + 输入 + OK + Cancel。
    #[must_use]
    pub fn input_ok_cancel(
        mut self,
        title: impl Into<SmolStr>,
        message: impl Into<SmolStr>,
        input: InputFactory,
    ) -> Self {
        self.kind = AlertKind::InputOkCancel;
        self.title = title.into();
        self.message = message.into();
        self.input = Some(input);
        self
    }

    /// OK 按钮文案。
    #[must_use]
    pub fn ok_text(mut self, text: impl Into<SmolStr>) -> Self {
        self.ok_text = text.into();
        self
    }

    /// Cancel 按钮文案。
    #[must_use]
    pub fn cancel_text(mut self, text: impl Into<SmolStr>) -> Self {
        self.cancel_text = text.into();
        self
    }

    /// 校验（点 OK 时跑；`Err` 显示错误并保持打开）。
    #[must_use]
    pub fn validate(mut self, validate: impl Fn(&str) -> Result<(), SmolStr> + 'static) -> Self {
        self.validate = Some(Rc::new(validate));
        self
    }

    /// OK 动作（关闭之后由调用方通过 P2-4b 的浮层执行，或直接在这里处理）。
    #[must_use]
    pub fn ok_action(mut self, action: impl Fn(Option<SmolStr>, &mut App) + 'static) -> Self {
        self.ok_action = Some(Rc::new(action));
        self
    }

    /// Cancel 动作。
    #[must_use]
    pub fn cancel_action(mut self, action: impl Fn(&mut App) + 'static) -> Self {
        self.cancel_action = Some(Rc::new(action));
        self
    }

    /// 「关闭中」守卫（crate 内部：由 [`crate::kit::dialog`] 挂载 alert 内容时注入）。
    ///
    /// 返回 `true`（容器已开始关闭、正在淡出）时内容按钮不再运行用户动作 —— 旧 dialog 在面板上
    /// `capture_any_mouse_down` 吞掉退场窗口内的按下，引擎化后内容按钮仍是活节点，由容器告知，
    /// 避免删除类确认在 150 ms 淡出窗口内被双击执行两次（P2-4d1 审查 D2；P2-4a3 修 alert 内容）。
    pub(crate) fn closing_guard(mut self, closing: impl Fn() -> bool + 'static) -> Self {
        self.closing = Some(Rc::new(closing));
        self
    }

    /// 建面板（标题 / 消息 / 输入 + 错误行 / 按钮行）。
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode` / 按钮或图标资源错误。
    pub fn build(
        self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        theme: &ThemeHandle,
    ) -> Result<Alert, EngineError> {
        let id = ALERT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let key = smol_str::format_smolstr!("alert.{id}");
        let scope = app.signal_scope();
        let error: Signal<Option<SmolStr>> = app.signal_scoped(scope, None);
        let title_color = theme.clone();
        let message_color = theme.clone();
        let track_theme = theme.clone();
        let track = StyleBuilder::new()
            .flex_col()
            .width(PANEL_WIDTH)
            .gap(8.0)
            .overflow_hidden()
            .build();
        let builder = NodeBuilder::boxed()
            .id(smol_str::format_smolstr!("{key}:panel"))
            .style(track)
            .bind_style(move |cx| StylePatch {
                foreground: Some(track_theme.role(cx, Role::FgDefault)),
                ..StylePatch::default()
            })
            .a11y(A11yOverride {
                // E12-R1：对话框角色只由 `kit::dialog` 的包装节点声明；内容根只是通用容器
                // （P2-4a 曾声明 `Dialog`，与包装层形成两层对话框角色，P2-4d1 审查 D6）。
                role: Some(A11yRole::GenericContainer),
                ..A11yOverride::default()
            });
        let root = builder.build(
            app.window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut(),
            parent,
        )?;
        let title_theme = title_color;
        NodeBuilder::text(self.title.clone(), {
            let mut style = TextStyle::new(20.0);
            style.weight = 700;
            style.alignment = TextAlign::Center;
            style
        })
        .bind_style(move |cx| StylePatch {
            foreground: Some(title_theme.role(cx, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(
            app.window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut(),
            root,
        )?;
        let msg_theme = message_color;
        // 旧实现的消息是 Markdown，继承窗口默认字号（16 px）；新实现是纯文本，按旧字号写死。
        NodeBuilder::text(self.message.clone(), TextStyle::new(16.0))
            .bind_style(move |cx| StylePatch {
                foreground: Some(msg_theme.role(cx, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                root,
            )?;
        let mut input_node = None;
        if let Some(factory) = &self.input {
            // 旧实现把输入框与错误行放在内层列里、列间距 4 px（旧 `.gap_1()`）。
            let input_column = NodeBuilder::boxed()
                .style(StyleBuilder::new().flex_col().gap(4.0).build())
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    root,
                )?;
            let input = factory(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                input_column,
            );
            let error_theme = theme.clone();
            NodeBuilder::text("", TextStyle::new(14.0))
                .bind_text(move |cx| cx.read(&error).clone().unwrap_or_else(SmolStr::default))
                .bind_style(move |cx| {
                    let visible = cx.read(&error).is_some();
                    StylePatch {
                        foreground: Some(error_theme.role(cx, Role::StatusDangerBase)),
                        visible: Some(visible),
                        ..StylePatch::default()
                    }
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    input_column,
                )?;
            input_node = Some(input);
        }
        let row = NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_row().width_full().gap(0.0).build())
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                root,
            )?;
        let button_count = if self.kind.with_cancel() { 2 } else { 1 };
        let ok_text = self.ok_text.clone();
        let validate = self.validate.clone();
        let ok_action = self.ok_action.clone();
        let error_signal = error;
        let input_for_ok = input_node.clone();
        let closing_for_ok = self.closing.clone();
        let ok_button = Button::new(app, theme.clone(), smol_str::format_smolstr!("{key}.ok"))
            .label(ok_text)
            .on_click(move |ctx: &mut ActionCtx<'_>| {
                if closing_for_ok.as_ref().is_some_and(|closing| closing()) {
                    return;
                }
                let value = input_for_ok
                    .as_ref()
                    .map(|input| SmolStr::new(ctx.app_mut().read(&input.state).text()));
                if let Some(validate) = &validate {
                    let checked = value.clone().unwrap_or_default();
                    if let Err(message) = validate(&checked) {
                        ctx.write(&error_signal, Some(message));
                        return;
                    }
                }
                ctx.write(&error_signal, None::<SmolStr>);
                if let Some(action) = &ok_action {
                    action(value, ctx.app_mut());
                }
            });
        let ok_button = if button_count == 2 {
            ok_button.joined(0, 2)
        } else {
            ok_button
        };
        let ok_node = ok_button.build(app, window, row)?;
        stretch_button(app, window, ok_node)?;
        let cancel_action = self.cancel_action.clone();
        let closing_for_cancel = self.closing.clone();
        let cancel_button = if self.kind.with_cancel() {
            let button = Button::new(
                app,
                theme.clone(),
                smol_str::format_smolstr!("{key}.cancel"),
            )
            .secondary()
            .label(self.cancel_text.clone())
            .joined(1, 2)
            .on_click(move |ctx: &mut ActionCtx<'_>| {
                if closing_for_cancel.as_ref().is_some_and(|closing| closing()) {
                    return;
                }
                if let Some(action) = &cancel_action {
                    action(ctx.app_mut());
                }
            });
            let node = button.build(app, window, row)?;
            stretch_button(app, window, node)?;
            Some(button)
        } else {
            None
        };
        Ok(Alert {
            root,
            title: self.title,
            ok_button,
            cancel_button,
            input: input_node,
            error,
            scope,
        })
    }
}

impl Default for AlertBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// 建好的 alert 面板。
pub struct Alert {
    root: NodeId,
    title: SmolStr,
    ok_button: Button,
    cancel_button: Option<Button>,
    input: Option<InputNode>,
    error: Signal<Option<SmolStr>>,
    scope: SignalScope,
}

impl Alert {
    /// 面板根节点（挂进 P2-4b 的对话框面板）。
    #[must_use]
    pub fn root_node(&self) -> NodeId {
        self.root
    }

    /// 标题（a11y 名 / 测试用）。
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// OK 按钮。
    #[must_use]
    pub fn ok_button(&self) -> &Button {
        &self.ok_button
    }

    /// Cancel 按钮（没有取消形状时 `None`）。
    #[must_use]
    pub fn cancel_button(&self) -> Option<&Button> {
        self.cancel_button.as_ref()
    }

    /// 输入框节点（没有输入形状时 `None`）。
    #[must_use]
    pub fn input(&self) -> Option<&InputNode> {
        self.input.as_ref()
    }

    /// 当前错误文案（`None` = 无错误）。
    #[must_use]
    pub fn error(&self, app: &App) -> Option<SmolStr> {
        app.read_with(&self.error, |error| error.clone())
    }

    /// 错误行是否可见。
    #[must_use]
    pub fn has_error(&self, app: &App) -> bool {
        self.error(app).is_some()
    }

    /// 帧同步（按钮状态；输入框由调用方按 `InputNode` 的契约同步）。
    ///
    /// # Errors
    /// 同 [`Button::sync`]。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.ok_button.sync(app)?;
        if let Some(cancel) = &self.cancel_button {
            changed |= cancel.sync(app)?;
        }
        Ok(changed)
    }

    /// 释放组件（按钮信号 / Action + 错误信号作用域）。
    pub fn release(&self, app: &mut App) -> usize {
        let buttons = self.ok_button.release(app)
            + self
                .cancel_button
                .as_ref()
                .map_or(0, |cancel| cancel.release(app));
        buttons + app.release_scope(self.scope)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        sync::Arc,
    };

    use ui_core::{
        a11y::A11yRole,
        event::{InputEvent, Modifiers, PointerButton},
        node::{id::NodeId, tree::NodeKind},
        paint::{color::ColorResolver, style::Clip},
        test_support::{app::TestApp, probe::Selector},
    };

    use super::{Alert, AlertBuilder, InputFactory};
    use crate::{
        kit::{
            input::{InputSpec, InputState, build as build_input},
            test_support::full_a11y,
        },
        theme::{
            appearance::Appearance, components::ControlSize, handle::ThemeHandle, resolved::Theme,
            role::Role,
        },
    };

    struct Harness {
        app: TestApp,
        alert: Alert,
    }

    fn input_factory(
        app: &mut TestApp,
        theme: &ThemeHandle,
        text: &str,
    ) -> (InputFactory, ui_core::node::signal::Signal<InputState>) {
        let spec = Arc::new(InputSpec {
            name: smol_str::SmolStr::new_static("alert-input"),
            placeholder: smol_str::SmolStr::default(),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: ControlSize::Medium,
            submit: None,
        });
        let state = app.app_mut().signal(InputState::new(text));
        let placeholder = app.app_mut().signal(spec.placeholder.clone());
        let theme = theme.clone();
        let factory_spec = spec;
        let factory = Rc::new(
            move |tree: &mut ui_core::node::tree::NodeTree, parent: NodeId| {
                build_input(
                    tree,
                    parent,
                    factory_spec.clone(),
                    &theme,
                    &state,
                    &placeholder,
                )
            },
        );
        (factory, state)
    }

    fn harness(
        configure: impl FnOnce(AlertBuilder, &mut TestApp, &ThemeHandle) -> AlertBuilder,
    ) -> Harness {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let builder = configure(AlertBuilder::new(), &mut app, &theme);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let alert = builder
            .build(app.app_mut(), window, root, &theme)
            .expect("build");
        let _ = app.drain().expect("frame");
        Harness { app, alert }
    }

    fn click(app: &mut TestApp, node: NodeId) {
        let bounds = app.probe().bounds(node).expect("bounds");
        let point = ui_core::math::Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
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
        let _ = app.drain().expect("frame");
    }

    /// 四种形状：取消 / 输入的组合与旧实现一致。
    #[test]
    fn the_four_alert_kinds_keep_their_old_shapes() {
        let shapes = [
            (super::AlertKind::Ok, false, false),
            (super::AlertKind::OkCancel, true, false),
            (super::AlertKind::InputOk, false, true),
            (super::AlertKind::InputOkCancel, true, true),
        ];
        for (kind, cancel, input) in shapes {
            assert_eq!(kind.with_cancel(), cancel, "{kind:?} 的取消按钮");
            assert_eq!(kind.with_input(), input, "{kind:?} 的输入框");
            let harness = harness(move |builder, app, theme| match kind {
                super::AlertKind::Ok => builder.ok("Saved", "The file has been written."),
                super::AlertKind::OkCancel => builder.ok_cancel("Discard", "Lose changes?"),
                super::AlertKind::InputOk => {
                    let (factory, _) = input_factory(app, theme, "name");
                    builder.input_ok("Rename", "Enter a name.", factory)
                }
                super::AlertKind::InputOkCancel => {
                    let (factory, _) = input_factory(app, theme, "");
                    builder.input_ok_cancel("New folder", "Name required.", factory)
                }
            });
            assert_eq!(harness.alert.cancel_button().is_some(), cancel);
            assert_eq!(harness.alert.input().is_some(), input);
            let window = harness.app.window_id();
            let buttons = harness
                .app
                .app()
                .window(window)
                .expect("window")
                .tree()
                .get(harness.alert.root_node())
                .expect("root")
                .children()
                .len();
            assert!(buttons >= 3, "标题 + 消息 + 按钮行");
        }
    }

    /// 旧断言恢复：单按钮铺满面板（按钮行是 `kit::alert` 自己建的，不归 P2-4b 面板）。
    #[test]
    fn an_alert_without_a_cancel_button_should_draw_one_filling_button() {
        let harness = harness(|builder, _, _| builder.ok("Saved", "The file has been written."));
        let panel = harness
            .app
            .probe()
            .bounds(harness.alert.root_node())
            .expect("panel");
        let ok = harness
            .app
            .probe()
            .bounds(harness.alert.ok_button().node().expect("ok"))
            .expect("ok bounds");
        assert_eq!(panel.size.width, 384.0, "面板宽度 w_96");
        assert_eq!(ok.size.width, panel.size.width, "单按钮铺满面板");
        assert_eq!(ok.origin.x, panel.origin.x, "按钮贴面板左缘");
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        assert_eq!(
            tree.get(harness.alert.root_node())
                .expect("panel")
                .style()
                .clip,
            Clip::Bounds,
            "面板裁剪子树（旧 overflow_hidden）"
        );
        let message = harness
            .app
            .probe()
            .by_selector(Selector::Text("The file has been written."))
            .expect("message");
        match tree.get(message).expect("message").kind() {
            NodeKind::Text(content) => {
                assert_eq!(content.style.size, 16.0, "消息继承旧 16 px 字号")
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    /// 旧断言恢复：两个按钮均分并铺满面板，内容居中（旧 `Join + flex_1 + justify_center`）。
    #[test]
    fn two_buttons_should_share_the_panel() {
        let harness = harness(|builder, _, _| builder.ok_cancel("Discard", "Lose changes?"));
        let probe = harness.app.probe();
        let panel = probe.bounds(harness.alert.root_node()).expect("panel");
        let ok_node = harness.alert.ok_button().node().expect("ok");
        let cancel_node = harness
            .alert
            .cancel_button()
            .expect("cancel")
            .node()
            .expect("cancel node");
        let ok = probe.bounds(ok_node).expect("ok bounds");
        let cancel = probe.bounds(cancel_node).expect("cancel bounds");
        assert!(
            (ok.size.width - cancel.size.width).abs() < 0.5,
            "两按钮等分（ok {} / cancel {}）",
            ok.size.width,
            cancel.size.width
        );
        assert!(
            (ok.size.width + cancel.size.width - panel.size.width).abs() < 0.5,
            "两按钮合计铺满面板（{} + {} vs {}）",
            ok.size.width,
            cancel.size.width,
            panel.size.width
        );
        assert_eq!(ok.origin.x, panel.origin.x, "OK 贴左缘");
        assert_eq!(
            cancel.origin.x + cancel.size.width,
            panel.origin.x + panel.size.width,
            "Cancel 贴右缘"
        );
        for (node, label) in [(ok_node, "OK"), (cancel_node, "Cancel")] {
            let bounds = probe.bounds(node).expect("button bounds");
            let text = probe
                .by_selector(Selector::Text(label))
                .unwrap_or_else(|| panic!("{label} 文字"));
            let text_bounds = probe.bounds(text).expect("text bounds");
            let center = text_bounds.origin.x + text_bounds.size.width / 2.0;
            let button_center = bounds.origin.x + bounds.size.width / 2.0;
            assert!(
                (center - button_center).abs() < 1.0,
                "{label} 的文字必须居中（{center} vs {button_center}）"
            );
        }
    }

    /// OK：跑 `ok_action(None)`（没有输入框）。
    #[test]
    fn the_ok_button_runs_the_ok_action_with_no_value() {
        let reported: Rc<RefCell<Option<Option<smol_str::SmolStr>>>> = Rc::new(RefCell::new(None));
        let sink = reported.clone();
        let harness = harness(move |builder, _, _| {
            builder
                .ok("Saved", "Written.")
                .ok_action(move |value, _| *sink.borrow_mut() = Some(value))
        });
        let mut app = harness.app;
        let node = harness.alert.ok_button().node().expect("ok node");
        click(&mut app, node);
        assert_eq!(
            reported.borrow().clone(),
            Some(None),
            "没有输入框时 ok_action 收到 None"
        );
    }

    /// Cancel：跑 `cancel_action`，不跑 OK 动作。
    #[test]
    fn the_cancel_button_runs_only_the_cancel_action() {
        let cancelled = Rc::new(Cell::new(0usize));
        let oks = Rc::new(Cell::new(0usize));
        let cancel_sink = cancelled.clone();
        let ok_sink = oks.clone();
        let harness = harness(move |builder, _, _| {
            builder
                .ok_cancel("Discard", "Lose changes?")
                .cancel_action(move |_| cancel_sink.set(cancel_sink.get() + 1))
                .ok_action(move |_, _| ok_sink.set(ok_sink.get() + 1))
        });
        let mut app = harness.app;
        let node = harness
            .alert
            .cancel_button()
            .expect("cancel")
            .node()
            .expect("cancel node");
        click(&mut app, node);
        assert_eq!(cancelled.get(), 1);
        assert_eq!(oks.get(), 0);
    }

    /// 输入形状：OK 收到输入框的当前值。
    #[test]
    fn the_ok_action_of_an_input_alert_receives_the_value() {
        let reported: Rc<RefCell<Option<Option<smol_str::SmolStr>>>> = Rc::new(RefCell::new(None));
        let sink = reported.clone();
        let harness = harness(move |builder, app, theme| {
            let (factory, _) = input_factory(app, theme, "old.txt");
            builder
                .input_ok("Rename", "Enter the new name.", factory)
                .ok_action(move |value, _| *sink.borrow_mut() = Some(value))
        });
        let mut app = harness.app;
        let node = harness.alert.ok_button().node().expect("ok node");
        click(&mut app, node);
        assert_eq!(
            reported.borrow().clone(),
            Some(Some(smol_str::SmolStr::new_static("old.txt")))
        );
    }

    /// 校验：空值被拒 → 错误可见、OK 动作不跑；改成合法值后通过。
    #[test]
    fn a_refused_value_keeps_the_alert_open_and_runs_nothing() {
        let calls = Rc::new(Cell::new(0usize));
        let sink = calls.clone();
        let harness = harness(move |builder, app, theme| {
            let (factory, _) = input_factory(app, theme, "");
            builder
                .input_ok("New folder", "Name required.", factory)
                .validate(|value| {
                    if value.is_empty() {
                        Err(smol_str::SmolStr::new_static("A name is required."))
                    } else {
                        Ok(())
                    }
                })
                .ok_action(move |_, _| sink.set(sink.get() + 1))
        });
        let mut app = harness.app;
        let ok = harness.alert.ok_button().node().expect("ok");
        click(&mut app, ok);
        assert!(harness.alert.has_error(app.app()), "空值应显示错误");
        assert_eq!(calls.get(), 0, "被拒的值不得调用 ok_action");

        let input = harness.alert.input().expect("input");
        input.update_value(app.app_mut(), "reports");
        click(&mut app, ok);
        assert!(!harness.alert.has_error(app.app()), "通过后错误清掉");
        assert_eq!(calls.get(), 1, "通过后调用 ok_action");
    }

    /// a11y：内容根只是通用容器（对话框角色归 `kit::dialog` 包装层），按钮是 Button。
    #[test]
    fn the_alert_content_root_is_a_generic_container_not_a_dialog() {
        let mut harness = harness(|builder, _, _| builder.ok("Saved", "Written."));
        let alert_root = harness.alert.root_node();
        let ok_node = harness.alert.ok_button().node().expect("ok node");
        let snapshot = full_a11y(&mut harness.app);
        let content = snapshot.node_for(alert_root).expect("内容根 a11y");
        assert_eq!(
            content.role,
            A11yRole::GenericContainer,
            "内容根不得声明 Dialog（P2-4d1 审查 D6）"
        );
        assert_eq!(content.label_text(), None, "可访问名归对话框包装层");
        let ok = snapshot.node_for(ok_node).expect("ok a11y");
        assert_eq!(ok.role, A11yRole::Button);
        assert_eq!(ok.label_text(), Some("OK"));
        assert!(
            snapshot
                .nodes()
                .iter()
                .all(|node| !matches!(node.role, A11yRole::Dialog | A11yRole::AlertDialog)),
            "alert 内容面板自身不含对话框角色"
        );
    }

    /// 调用方给的文案（本地化）同时是按钮可见文字与 a11y 名；默认英文不得残留。
    #[test]
    fn the_alert_uses_the_callers_localized_button_labels() {
        let harness = harness(|builder, _, _| {
            builder
                .ok_cancel("删除", "此操作不可撤销。")
                .ok_text("删除")
                .cancel_text("取消")
        });
        let ok_node = harness.alert.ok_button().node().expect("ok node");
        let cancel_node = harness
            .alert
            .cancel_button()
            .expect("cancel")
            .node()
            .expect("cancel node");
        let mut app = harness.app;
        let snapshot = full_a11y(&mut app);
        for (node, label) in [(ok_node, "删除"), (cancel_node, "取消")] {
            let record = snapshot.node_for(node).expect("按钮 a11y");
            assert_eq!(record.role, A11yRole::Button);
            assert_eq!(record.label_text(), Some(label), "a11y 名用调用方文案");
        }
        let probe = app.probe();
        for label in ["删除", "取消"] {
            assert!(
                probe.by_selector(Selector::Text(label)).is_some(),
                "{label} 必须是按钮的可见文字"
            );
        }
        for default in ["OK", "Cancel"] {
            assert!(
                probe.by_selector(Selector::Text(default)).is_none(),
                "调用方给了本地化文案时不得残留默认英文 {default}"
            );
        }
    }

    /// `closing_guard`：容器开始关闭后，OK / Cancel 都不再跑用户动作（dialog 集成测试另见
    /// `kit::dialog::a_second_click_on_alert_content_during_the_fade_runs_no_action`）。
    #[test]
    fn a_closing_alert_content_runs_no_button_actions() {
        let closing = Rc::new(Cell::new(false));
        let ok_runs = Rc::new(Cell::new(0u32));
        let cancel_runs = Rc::new(Cell::new(0u32));
        let closing_for_builder = closing.clone();
        let ok_for_builder = ok_runs.clone();
        let cancel_for_builder = cancel_runs.clone();
        let harness = harness(move |builder, _, _| {
            builder
                .ok_cancel("Discard", "Lose changes?")
                .ok_action(move |_, _| ok_for_builder.set(ok_for_builder.get() + 1))
                .cancel_action(move |_| cancel_for_builder.set(cancel_for_builder.get() + 1))
                .closing_guard(move || closing_for_builder.get())
        });
        let mut app = harness.app;
        let ok = harness.alert.ok_button().node().expect("ok node");
        let cancel = harness
            .alert
            .cancel_button()
            .expect("cancel")
            .node()
            .expect("cancel node");
        click(&mut app, ok);
        assert_eq!(ok_runs.get(), 1, "未关闭时 OK 动作照常跑");
        closing.set(true);
        click(&mut app, ok);
        assert_eq!(ok_runs.get(), 1, "关闭中不得再跑 OK 动作");
        click(&mut app, cancel);
        assert_eq!(cancel_runs.get(), 0, "关闭中不得跑 Cancel 动作");
    }

    /// 错误行初始不可见（`visible = false`），错误出现后可见。
    #[test]
    fn the_error_line_is_hidden_until_a_refusal() {
        let harness = harness(|builder, app, theme| {
            let (factory, _) = input_factory(app, theme, "");
            builder
                .input_ok("Rename", "Enter a name.", factory)
                .validate(|_| Err(smol_str::SmolStr::new_static("Nope")))
        });
        let mut app = harness.app;
        let ok = harness.alert.ok_button().node().expect("ok");
        click(&mut app, ok);
        assert!(harness.alert.has_error(app.app()));
        let window = app.window_id();
        let tree = app.app().window(window).expect("window").tree();
        let error = app
            .probe()
            .by_selector(Selector::Text("Nope"))
            .expect("错误行文本");
        assert_eq!(
            tree.get(error).expect("error").style_patch().visible,
            Some(true),
            "错误出现后错误行可见"
        );
        // 旧语义：错误行 `text_sm`（14 px）+ `status.danger.base` 前景；与输入框同列、列间距 4 px。
        match tree.get(error).expect("error").kind() {
            NodeKind::Text(content) => assert_eq!(content.style.size, 14.0, "错误行字号 14 px"),
            other => panic!("expected Text, got {other:?}"),
        }
        let danger = Theme::of(Appearance::Light).role(Role::StatusDangerBase);
        assert_eq!(
            tree.get(error)
                .expect("error")
                .style_patch()
                .foreground
                .map(|color| ColorResolver.resolve(color).components),
            Some(ColorResolver.resolve(danger).components),
            "错误行必须用 danger 前景"
        );
        let column = tree
            .get(error)
            .expect("error")
            .parent()
            .expect("error column");
        assert_eq!(
            tree.get(column).expect("column").style().inner.gap.height,
            taffy::style::LengthPercentage::length(4.0),
            "输入框与错误行的列间距 4 px（旧 gap_1）"
        );
    }
}

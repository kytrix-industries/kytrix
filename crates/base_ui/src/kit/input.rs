//! 应用层文本编辑状态机 + 引擎（`ui_core`）渲染（P2-3）。
//!
//! 旧 `crate::input` 是 gpui 之上的一层壳：值/掩码/禁用/只读/占位符/光标与选区全部委托给
//! gpui-base 的 11,955 行编辑状态机。引擎化之后**编辑状态机归应用层**（本模块），引擎只提供
//! 三件事：文本 shaping 与几何查询（`Window::text_caret_rect` / `Window::text_byte_index_at` /
//! `NodeTree::text_selection_rects`）、逐帧失效与绘制（`bind_text` / `bind_style` /
//! `Window::set_canvas`）、以及输入路由（`App::drain_events` / IME caret hint）。
//!
//! # 数据流与外壳义务
//!
//! ```text
//! 外壳每轮：drain_events → dispatch(逐条) → sync → 引擎帧（styles → text → layout → paint）
//!                                  │            │
//!                                  ▼            ▼
//!                     Signal<InputState>  ←─ 焦点镜像 / IME 开关与光标区 / caret+选区 Canvas
//! ```
//!
//! 本模块**不装 `App::on_frame`**（引擎只有一个钩子，归外壳所有）。外壳需要：
//!
//! 1. [`InputNode::install`]：挂载后调用一次，注册点击定位处理器；
//! 2. `drain_events` 之后把每条 `Event` 交给 [`InputNode::dispatch`]；
//! 3. 每轮调用一次 [`InputNode::sync`]（推荐放在外壳自己的 `on_frame` 钩子里，即帧之前）：
//!    镜像引擎焦点、开关 IME、把 caret/选区画布交给引擎。
//!
//! 编辑产生的 `Change` / `Blur` 由外壳**读状态信号**观察（`InputState::text` / `is_focused` /
//! `error`）；`PressEnter` 通过 [`InputSpec::submit`] 动作表达（单行输入在焦点节点上派发）。
//! 图标 / 清空按钮 / 密码可见性切换都依赖「图标与资源」批次，本模块不引入。
//!
//! # 已知边界（引擎没有公开 API，不做变通实现）
//!
//! - **字素簇级 Backspace/Delete**：引擎的 grapheme/cluster 划分只在 a11y 内部的
//!   `pub(crate)` 模块里（`a11y::textrun`），公开面没有「字节 → 簇边界」查询。本模块按 `char`
//!   删除并保证**永不切断 UTF-8**；组合附加符号 / ZWJ 序列会被拆成多个 `char`。
//! - **上下方向键**：视觉行不是公开 API（`ShapedLine` 只在 crate 内可达），因此 `↑/↓` 不消费
//!   （不伪造「跳到段首/段尾」的语义）。左右方向键 / Home / End / Ctrl+Home / Ctrl+End 可用。
//! - **按住拖选**：引擎的指针捕获/拖拽 Action 面向「拖拽有业务语义的控件」；本模块只做点击
//!   定位，不做按住拖选。
//! - **密码框 IME 语义**：Linux 的 `Window::set_ime_content_type(ImeContentType::Password)`
//!   是窗口级接口且属平台批次，未接线。
//! - **多输入同窗的 IME 开关**：引擎的 `set_ime_enabled` 是**窗口级**状态。本模块只在
//!   「本输入框失焦」时关闭、在聚焦时打开，因此同窗多个输入框时以**最后同步者**为准；外壳应
//!   在焦点转移的那一轮按「先同步失焦者、后同步得焦者」调用 `sync`（或只同步聚焦控件）。

use std::{ops::Range, sync::Arc};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::LengthPercentageAuto;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    event::{
        self, Action, ActionSource, Event, ImeEvent, InputEvent, KeyCode, KeyState, Modifiers,
    },
    math::{Point, Rect},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::Signal,
        tree::{NodeKind, NodeTree},
    },
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{Border, Quad, QuadPaint},
        style::{Cursor, StylePatch},
    },
    text::{TextAlign, TextOverflow, TextStyle, TextWrap},
    window::{Window, WindowId},
};

use crate::{
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{components::ControlSize, resolved::Theme, role::Role},
};

/// 掩码字符（密码框逐字符显示）。
const BULLET: char = '•';
/// 输入框圆角（与旧 `rounded_sm` 的 4 px 一致；主题 token 暂无圆角档位）。
const CORNER_RADIUS: f32 = 4.0;

/// 一节输入框的构造参数（挂载期常量；主题/尺寸变化 = 重新挂载）。
#[derive(Clone, Debug)]
pub struct InputSpec {
    /// 节点 id 与动作 key 的前缀（**同一 App 内必须唯一**），同时作为 a11y 的兜底标签。
    pub name: SmolStr,
    /// 空值占位符。
    pub placeholder: SmolStr,
    /// 密码框：逐字符显示 `•`，禁用复制/剪切。
    pub masked: bool,
    /// 多行：Enter 插入换行、文本按宽度换行。
    pub multiline: bool,
    /// 只读：光标/选区/复制可用，一切修改被拒。
    pub readonly: bool,
    /// 禁用：不可聚焦、不可命中、不派发点击动作。
    pub disabled: bool,
    /// 最大字符数（`char` 计数，不是字节）。
    pub max_length: Option<usize>,
    /// 控件尺寸档（高度 / 字号 / 内边距取自主题 token）。
    pub size: ControlSize,
    /// 单行输入在焦点节点上按 Enter 时派发的动作（`None` = 只消费按键）。
    pub submit: Option<Action>,
}

/// 输入框的强类型错误。
///
/// `i18n_key` 是稳定的翻译 key：`Empty` / `TooLong` 由本模块产生，`Invalid` 的 key 由应用
/// 校验后经 [`InputState::set_error`] 写入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    /// 空值（应用在某次 `validate` 里要求非空）。
    Empty,
    /// 超过最大字符数。
    TooLong {
        /// 上限（字符数）。
        max: usize,
    },
    /// 应用自己的校验失败（携带自己的 i18n key）。
    Invalid {
        /// 应用的翻译 key。
        i18n_key: SmolStr,
    },
}

impl InputError {
    /// 翻译 key（错误态渲染/播报的唯一入口）。
    #[must_use]
    pub fn i18n_key(&self) -> &str {
        match self {
            Self::Empty => "input.error.empty",
            Self::TooLong { .. } => "input.error.tooLong",
            Self::Invalid { i18n_key } => i18n_key.as_str(),
        }
    }
}

/// 纯状态层的光标移动（不依赖 `InputSpec`；Home/End 是**行级**，单行时等价文档首尾）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaretMotion {
    /// 左移一个 `char`。
    CharLeft,
    /// 右移一个 `char`。
    CharRight,
    /// 当前行行首（上一个 `\n` 之后）。
    Home,
    /// 当前行行尾（下一个 `\n` 之前）。
    End,
    /// 文档首。
    DocStart,
    /// 文档尾。
    DocEnd,
}

/// 一次 IME 组合的状态。
///
/// **不写进提交文本**：组合串是显示层叠加（`display_text` = 前缀 + preedit + 后缀），提交时才
/// 进入 [`InputState`] 的正文。`caret_utf16` 是**组合串内**的插入点（UTF-16 码元）。
#[derive(Clone, Debug, PartialEq)]
struct Composition {
    /// 组合起点（提交文本的 UTF-8 字节偏移）。
    start: usize,
    /// 组合串（UTF-8）。
    text: String,
    /// 组合串内插入点（UTF-16 码元，相对 `text`）；`None` = 画在末尾。
    caret_utf16: Option<u32>,
}

/// 输入框的编辑状态（纯数据；`Signal<InputState>` 是唯一真值）。
///
/// 坐标空间：
/// - [`InputState::caret`] 与 [`InputState::display_text`] 用**未脱敏显示文本**的 UTF-8 字节
///   （组合期 preedit 内联在 `display_text` 里，因此 caret 可以直接喂给引擎的
///   `text_caret_rect` / `set_ime_cursor_area`；脱敏由本模块在 `sync` 里按字符数换算）；
/// - [`InputState::selection`] / [`InputState::selection_anchor`] 用**提交文本**的 UTF-8 字节；
///   组合期选区恒为空（`start..start`）。
#[derive(Clone, Debug, PartialEq)]
pub struct InputState {
    /// 已提交正文。
    text: String,
    /// 插入点（提交文本 UTF-8 字节，恒在 char boundary）。
    caret: usize,
    /// 选区锚点（提交文本 UTF-8 字节，恒在 char boundary）。
    anchor: usize,
    /// 当前 IME 组合（`None` = 未在组合）。
    composition: Option<Composition>,
    /// 焦点镜像（由外壳每轮 `sync` 从引擎焦点写回）。
    focused: bool,
    /// 最近一次校验/应用写入的错误。
    error: Option<InputError>,
}

impl InputState {
    /// 以初始正文构造（光标在末尾、无选区）。
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            caret: end,
            anchor: end,
            composition: None,
            focused: false,
            error: None,
        }
    }

    /// 已提交正文（**不含** IME 组合串）。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 显示文本：已提交正文 + 内联 preedit；`masked` 时逐字符替换为 `•`（不是逐字节）。
    #[must_use]
    pub fn display_text(&self, masked: bool) -> SmolStr {
        let unmasked = self.display_unmasked();
        if !masked {
            return unmasked;
        }
        let chars = unmasked.chars().count();
        let mut bullets = String::with_capacity(chars * BULLET.len_utf8());
        bullets.extend(std::iter::repeat_n(BULLET, chars));
        SmolStr::new(bullets)
    }

    /// 插入点的**未脱敏显示文本** UTF-8 字节（组合期 = 组合起点 + 组合串内 caret）。
    #[must_use]
    pub fn caret(&self) -> usize {
        self.display_caret_byte(false)
    }

    /// 选区（提交文本字节，升序）；组合期为空区间（`start..start`）。
    #[must_use]
    pub fn selection(&self) -> Range<usize> {
        if self.composition.is_some() {
            let start = self.composition.as_ref().map_or(0, |comp| comp.start);
            return start..start;
        }
        self.selection_range()
    }

    /// 选区锚点（提交文本字节）；组合期 = 组合起点。
    #[must_use]
    pub fn selection_anchor(&self) -> usize {
        match &self.composition {
            Some(comp) => comp.start,
            None => self.anchor,
        }
    }

    /// 是否正在 IME 组合。
    #[must_use]
    pub fn is_composing(&self) -> bool {
        self.composition.is_some()
    }

    /// 焦点镜像（引擎焦点的上一轮同步结果；权威源仍是 `App::focused`）。
    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// 当前错误（错误态渲染读它）。
    #[must_use]
    pub fn error(&self) -> Option<&InputError> {
        self.error.as_ref()
    }

    /// 插入文本（替换选区；先提交组合）。`readonly` / `disabled` 直接拒绝；`max_length`
    /// 按字符裁剪（永不 panic、永不切开字符）。
    ///
    /// 返回正文/选区是否变化。
    pub fn insert(&mut self, spec: &InputSpec, text: &str) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        let committed = self.commit_composition(spec);
        let replaced = self.replace_selection_with(spec, text);
        committed || replaced
    }

    /// 退格：先消掉组合（不删组合文本），否则删选区，再否则删光标前一个 `char`。
    pub fn delete_backward(&mut self, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        if self.cancel_composition() {
            return true;
        }
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        if start != end {
            return self.delete_selection();
        }
        let caret = snap_char_boundary(&self.text, self.caret);
        self.caret = caret;
        self.anchor = caret;
        let Some(prev) = prev_char_boundary(&self.text, caret) else {
            return false;
        };
        self.text.replace_range(prev..caret, "");
        self.caret = prev;
        self.anchor = prev;
        self.error = None;
        true
    }

    /// 删除：先消掉组合，否则删选区，再否则删光标后一个 `char`。
    pub fn delete_forward(&mut self, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        if self.cancel_composition() {
            return true;
        }
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        if start != end {
            return self.delete_selection();
        }
        let caret = snap_char_boundary(&self.text, self.caret);
        self.caret = caret;
        self.anchor = caret;
        let Some(next) = next_char_boundary(&self.text, caret) else {
            return false;
        };
        self.text.replace_range(caret..next, "");
        self.error = None;
        true
    }

    /// 插入换行（仅多行；其余情况 `false`）。
    pub fn insert_newline(&mut self, spec: &InputSpec) -> bool {
        if !spec.multiline {
            return false;
        }
        self.insert(spec, "\n")
    }

    /// 全选（取消组合）。
    pub fn select_all(&mut self) {
        self.cancel_composition();
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// 设置插入点（向下吸附到 char boundary 并 clamp）；`extend` = 保留锚点（扩选）。
    pub fn set_caret(&mut self, byte: usize, extend: bool) {
        self.cancel_composition();
        let byte = snap_char_boundary(&self.text, byte);
        if extend {
            self.caret = byte;
        } else {
            self.anchor = byte;
            self.caret = byte;
        }
    }

    /// 移动插入点（取消组合）。Home/End 是**行级**（找当前行两侧的 `\n`）。
    pub fn move_caret(&mut self, motion: CaretMotion, extend: bool) {
        self.cancel_composition();
        let caret = snap_char_boundary(&self.text, self.caret);
        let target = match motion {
            CaretMotion::CharLeft => prev_char_boundary(&self.text, caret).unwrap_or(0),
            CaretMotion::CharRight => {
                next_char_boundary(&self.text, caret).unwrap_or(self.text.len())
            }
            CaretMotion::Home => self.text[..caret].rfind('\n').map_or(0, |index| index + 1),
            CaretMotion::End => self.text[caret..]
                .find('\n')
                .map_or(self.text.len(), |index| caret + index),
            CaretMotion::DocStart => 0,
            CaretMotion::DocEnd => self.text.len(),
        };
        if extend {
            self.caret = target;
        } else {
            self.anchor = target;
            self.caret = target;
        }
    }

    /// 更新 IME 组合串（整串替换当前组合串；`caret_utf16` 相对 `text`）。
    ///
    /// 没有进行中的组合时从光标处开始（先消掉选区）。`readonly` / `disabled` ⇒ `false`。
    pub fn ime_preedit(&mut self, spec: &InputSpec, text: &str, caret_utf16: Option<u32>) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        let started = if self.composition.is_none() {
            if !self.ime_start_inner(spec) {
                return false;
            }
            true
        } else {
            false
        };
        let Some(composition) = self.composition.as_mut() else {
            return false;
        };
        let previous_text = composition.text.clone();
        let previous_caret = composition.caret_utf16;
        let units = utf16_len(text) as u32;
        composition.text.clear();
        composition.text.push_str(text);
        composition.caret_utf16 = caret_utf16.map(|caret| caret.min(units));
        started || previous_text != composition.text || previous_caret != composition.caret_utf16
    }

    /// 提交组合文本：`text` 替换整个当前组合串并进入正文（先按 `max_length` 裁剪）。
    ///
    /// 没有组合时等价于 [`InputState::insert`]。
    pub fn ime_commit(&mut self, spec: &InputSpec, text: &str) -> bool {
        if spec.disabled || spec.readonly {
            self.ime_cancel();
            return false;
        }
        let start = self.composition.as_ref().map(|comp| comp.start);
        if let Some(start) = start {
            self.composition = None;
            self.caret = start;
            self.anchor = start;
        }
        let inserted = self.insert(spec, text);
        start.is_some() || inserted
    }

    /// 取消组合（丢弃 preedit，插入点回到组合起点）。
    pub fn ime_cancel(&mut self) {
        self.cancel_composition();
    }

    /// 复制选区到剪贴板（脱敏字段 / 无选区 / 禁用 ⇒ `false`；只读允许复制）。
    pub fn copy(&self, app: &App, spec: &InputSpec) -> bool {
        if spec.disabled || spec.masked {
            return false;
        }
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        if start == end {
            return false;
        }
        app.set_clipboard_text(&self.text[start..end]).is_ok()
    }

    /// 剪切选区（脱敏 / 只读 / 禁用 ⇒ `false`）。
    pub fn cut(&mut self, app: &App, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly || spec.masked {
            return false;
        }
        if !self.copy(app, spec) {
            return false;
        }
        self.delete_selection()
    }

    /// 粘贴剪贴板文本（先提交组合；`readonly` / `disabled` ⇒ `false`）。
    pub fn paste(&mut self, app: &App, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        let Ok(text) = app.clipboard_text() else {
            return false;
        };
        self.insert(spec, text.as_str())
    }

    /// 写回焦点镜像（失焦时丢弃未提交的组合）。
    pub fn set_focused(&mut self, focused: bool) {
        if !focused {
            self.cancel_composition();
        }
        self.focused = focused;
    }

    /// 用 `max_length` + 空值做一次校验并写入错误态。
    ///
    /// 只做这两条（应用自己的领域校验经 [`InputState::set_error`] 写入）。
    pub fn validate(&mut self, spec: &InputSpec) -> Option<InputError> {
        let error = if self.text.is_empty() {
            Some(InputError::Empty)
        } else {
            spec.max_length
                .filter(|max| self.text.chars().count() > *max)
                .map(|max| InputError::TooLong { max })
        };
        self.error = error.clone();
        error
    }

    /// 应用写入错误态（`None` = 清空）。
    pub fn set_error(&mut self, error: Option<InputError>) {
        self.error = error;
    }

    /// 引擎感知：把文本节点上的局部点换算成插入点（`extend` = Shift 扩选）。
    ///
    /// 入参与 `Window::text_byte_index_at` 同域（文本节点 border box 局部）；脱敏显示与组合串
    /// 都由本方法按字符数映射回真实正文。文本尚未 shape ⇒ `false`（等下一帧再定位）。
    pub fn set_caret_from_point(
        &mut self,
        window: &Window,
        text_node: NodeId,
        local_point: Point,
        extend: bool,
    ) -> bool {
        let Ok(display_byte) = window.text_byte_index_at(text_node, local_point) else {
            return false;
        };
        let Ok(node) = window.tree().get(text_node) else {
            return false;
        };
        let NodeKind::Text(content) = node.kind() else {
            return false;
        };
        let display = content.text.as_str();
        let display_byte = snap_char_boundary(display, display_byte);
        let char_index = display[..display_byte].chars().count();
        let target = self.committed_offset_for_display_char(char_index);
        let composition_dropped = self.composition.take().is_some();
        let moved = if extend {
            if self.caret == target {
                false
            } else {
                self.caret = target;
                true
            }
        } else if self.anchor == target && self.caret == target {
            false
        } else {
            self.anchor = target;
            self.caret = target;
            true
        };
        let changed = composition_dropped || moved;
        if changed {
            self.error = None;
        }
        changed
    }

    /// 引擎感知：键盘路由（Ctrl/Cmd+A/C/X/V、Backspace/Delete、←/→ + Shift、Home/End、
    /// Enter）。返回是否消费该按键；`Space` 由 `InputEvent::Text` 插入，不在这里处理。
    ///
    /// 组合期只处理 `Escape`（取消组合），其余按键交给平台/输入法。
    pub fn handle_key(
        &mut self,
        app: &mut App,
        window: WindowId,
        spec: &InputSpec,
        code: KeyCode,
        modifiers: Modifiers,
    ) -> bool {
        if spec.disabled || !self.focused {
            return false;
        }
        if self.composition.is_some() {
            if matches!(code, KeyCode::Escape) {
                self.ime_cancel();
                return true;
            }
            return false;
        }
        let command = modifiers.contains(Modifiers::CONTROL) || modifiers.contains(Modifiers::META);
        let extend = modifiers.contains(Modifiers::SHIFT);
        match code {
            KeyCode::KeyA if command => {
                self.select_all();
                true
            }
            KeyCode::KeyC if command => {
                self.copy(app, spec);
                true
            }
            KeyCode::KeyX if command => {
                self.cut(app, spec);
                true
            }
            KeyCode::KeyV if command => {
                self.paste(app, spec);
                true
            }
            KeyCode::Backspace if !command => {
                self.delete_backward(spec);
                true
            }
            KeyCode::Delete if !command => {
                self.delete_forward(spec);
                true
            }
            KeyCode::ArrowLeft if !command => {
                self.move_caret(CaretMotion::CharLeft, extend);
                true
            }
            KeyCode::ArrowRight if !command => {
                self.move_caret(CaretMotion::CharRight, extend);
                true
            }
            KeyCode::Home => {
                let motion = if command {
                    CaretMotion::DocStart
                } else {
                    CaretMotion::Home
                };
                self.move_caret(motion, extend);
                true
            }
            KeyCode::End => {
                let motion = if command {
                    CaretMotion::DocEnd
                } else {
                    CaretMotion::End
                };
                self.move_caret(motion, extend);
                true
            }
            KeyCode::Enter | KeyCode::NumpadEnter if !command => {
                if spec.multiline {
                    self.insert_newline(spec);
                } else if let Some(submit) = &spec.submit {
                    app.dispatch_action_focused(window, submit.clone(), ActionSource::Key);
                }
                true
            }
            _ => false,
        }
    }

    /// 引擎感知：IME 事件路由。`replace` 区间（UTF-16，相对当前组合串起点）在这里折算成
    /// 「整串替换」再交给状态层；`End` 在没收到 `Commit` 时等价取消。
    ///
    /// 返回是否消费该事件。
    pub fn handle_ime(
        &mut self,
        app: &mut App,
        window: WindowId,
        spec: &InputSpec,
        event: &ImeEvent,
    ) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        let changed = match event {
            ImeEvent::Start => self.ime_start_inner(spec),
            ImeEvent::Preedit {
                text,
                caret,
                replace,
                ..
            } => {
                let (full, caret_full) = self.preedit_replaced(text, *caret, replace.as_ref());
                self.ime_preedit(spec, &full, caret_full)
            }
            ImeEvent::Commit { text, replace } => match replace {
                Some(range) if self.composition.is_some() => {
                    let (full, _) = self.preedit_replaced(text, None, Some(range));
                    self.ime_commit(spec, &full)
                }
                _ => self.ime_commit(spec, text),
            },
            ImeEvent::End => {
                let composing = self.composition.is_some();
                if composing {
                    self.ime_cancel();
                }
                composing
            }
        };
        if changed {
            let _ = app.request_repaint(window);
        }
        true
    }

    /// 未脱敏显示文本（正文 + 内联 preedit）。
    fn display_unmasked(&self) -> SmolStr {
        match &self.composition {
            Some(composition) => format_smolstr!(
                "{}{}{}",
                &self.text[..composition.start],
                composition.text,
                &self.text[composition.start..]
            ),
            None => SmolStr::new(self.text.as_str()),
        }
    }

    /// 组合串内的 caret（组合串 UTF-8 字节）。
    fn preedit_caret_utf8(&self) -> usize {
        let Some(composition) = &self.composition else {
            return 0;
        };
        let units = composition.caret_utf16.map_or_else(
            || utf16_len(&composition.text) as u16,
            |caret| u16::try_from(caret).unwrap_or(u16::MAX),
        );
        utf8_from_utf16_offset(&composition.text, units)
    }

    /// 提交文本偏移 → 未脱敏显示文本偏移（组合串内联展开）。
    fn display_offset(&self, committed: usize) -> usize {
        match &self.composition {
            Some(composition) if committed > composition.start => {
                committed + composition.text.len()
            }
            _ => committed,
        }
    }

    /// 插入点在显示文本里的字节偏移（`masked` 时按「一字符 = 一个 `•`」换算）。
    fn display_caret_byte(&self, masked: bool) -> usize {
        let unmasked = match &self.composition {
            Some(composition) => composition.start + self.preedit_caret_utf8(),
            None => self.caret,
        };
        if masked {
            masked_offset(&self.display_unmasked(), unmasked)
        } else {
            unmasked
        }
    }

    /// 选区在显示文本里的字节区间（组合期为空）。
    fn display_selection(&self, masked: bool) -> Range<usize> {
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        let display_start = self.display_offset(start);
        let display_end = self.display_offset(end);
        if masked {
            let display = self.display_unmasked();
            masked_offset(&display, display_start)..masked_offset(&display, display_end)
        } else {
            display_start..display_end
        }
    }

    /// 提交文本里的选区（升序）。
    fn selection_range(&self) -> Range<usize> {
        if self.anchor <= self.caret {
            self.anchor..self.caret
        } else {
            self.caret..self.anchor
        }
    }

    /// 删除当前选区（提交文本）；返回是否真的删了。
    fn delete_selection(&mut self) -> bool {
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        if start == end {
            return false;
        }
        self.text.replace_range(start..end, "");
        self.caret = start;
        self.anchor = start;
        self.error = None;
        true
    }

    /// 用 `text` 替换选区（`max_length` 按字符裁剪）。
    fn replace_selection_with(&mut self, spec: &InputSpec, text: &str) -> bool {
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        let selected = self.text[start..end].chars().count();
        let total = self.text.chars().count();
        let budget = spec
            .max_length
            .map_or(usize::MAX, |max| max.saturating_sub(total - selected));
        let clipped: String = text.chars().take(budget).collect();
        if clipped.is_empty() && start == end {
            return false;
        }
        self.text.replace_range(start..end, &clipped);
        let caret = start + clipped.len();
        self.caret = caret;
        self.anchor = caret;
        self.error = None;
        true
    }

    /// 提交当前组合（`true` = 有过组合）。
    fn commit_composition(&mut self, spec: &InputSpec) -> bool {
        let Some(composition) = self.composition.take() else {
            return false;
        };
        self.caret = composition.start;
        self.anchor = composition.start;
        self.replace_selection_with(spec, &composition.text);
        true
    }

    /// 开始组合（清掉选区后把组合起点放在插入点）。已有组合 / 只读 / 禁用 ⇒ `false`。
    fn ime_start_inner(&mut self, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly || self.composition.is_some() {
            return false;
        }
        self.delete_selection();
        let start = snap_char_boundary(&self.text, self.caret);
        self.caret = start;
        self.anchor = start;
        self.composition = Some(Composition {
            start,
            text: String::new(),
            caret_utf16: None,
        });
        true
    }

    /// 取消组合（插入点回到组合起点）；`true` = 之前确实在组合。
    fn cancel_composition(&mut self) -> bool {
        let Some(composition) = self.composition.take() else {
            return false;
        };
        self.caret = composition.start;
        self.anchor = composition.start;
        true
    }

    /// 把「组合串内替换区间 + 新片段」折算成整串 preedit，并把 caret 换算到整串坐标。
    ///
    /// `replace` 是 UTF-16 码元、相对**当前组合串起点**；`None` = 整个当前组合串。caret 随
    /// 片段走：最终 caret = 前缀 UTF-16 长度 + 片段内 caret。
    fn preedit_replaced(
        &self,
        fragment: &str,
        caret_fragment: Option<u32>,
        replace: Option<&Range<usize>>,
    ) -> (String, Option<u32>) {
        let current = self
            .composition
            .as_ref()
            .map_or("", |composition| composition.text.as_str());
        let total = utf16_len(current);
        let (start_units, end_units) = match replace {
            Some(range) => (range.start.min(total), range.end.min(total)),
            None => (0, total),
        };
        let start_units = start_units.min(end_units);
        let end_units = end_units.max(start_units);
        let start_byte = utf8_from_utf16_units(current, start_units);
        let end_byte = utf8_from_utf16_units(current, end_units);
        let start_byte = start_byte.min(end_byte);
        let end_byte = end_byte.max(start_byte);
        let mut full =
            String::with_capacity(start_byte + fragment.len() + current.len() - end_byte);
        full.push_str(&current[..start_byte]);
        full.push_str(fragment);
        full.push_str(&current[end_byte..]);
        let caret_full = caret_fragment.map(|caret| {
            let fragment_units = utf16_len(fragment) as u32;
            let local = caret.min(fragment_units) as usize;
            (utf16_len(&current[..start_byte]) + local) as u32
        });
        (full, caret_full)
    }

    /// 显示文本（未脱敏）里的第 `char_index` 个字符位置 → 提交文本字节偏移。
    ///
    /// 落在组合串内部时收敛到组合起点（点击 preedit 视为「放到组合开始处」）。
    fn committed_offset_for_display_char(&self, char_index: usize) -> usize {
        let Some(composition) = &self.composition else {
            return byte_at_char(&self.text, char_index);
        };
        let prefix = &self.text[..composition.start];
        let prefix_chars = prefix.chars().count();
        if char_index <= prefix_chars {
            return byte_at_char(prefix, char_index);
        }
        let preedit_chars = composition.text.chars().count();
        if char_index < prefix_chars + preedit_chars {
            return composition.start;
        }
        byte_at_char(&self.text, char_index - preedit_chars)
    }
}

/// 挂载一次输入框后返回的节点句柄与状态（所有字段在 [`build`] 里一次确定）。
#[derive(Clone, Debug)]
pub struct InputNode {
    /// 可聚焦容器：背景 / 边框 / 内边距 / 点击动作。
    pub root: NodeId,
    /// 文本节点（`bind_text` 读显示文本）。
    pub text: NodeId,
    /// 占位符节点（空值可见，`opacity` 切换）。
    pub placeholder: NodeId,
    /// caret / 选区 Canvas 节点（`sync` 用 `Window::set_canvas` 换内容）。
    pub caret: NodeId,
    /// 编辑状态信号（唯一真值）。
    pub state: Signal<InputState>,
    /// 构造参数（`Arc`：点击处理器与订阅方共享）。
    pub spec: Arc<InputSpec>,
    /// caret 颜色（挂载期从主题解析）。
    caret_color: Color,
    /// 选区颜色（挂载期从主题解析）。
    selection_color: Color,
}

/// 在 `tree` 的 `parent` 下挂载输入框，返回节点句柄。
///
/// # Panics
/// `parent` 已卸载（陈旧句柄）时 panic：挂载契约与 `NodeBuilder::build` 相同，调用方必须给
/// 活着的父节点。引擎没有可伪造的 `NodeId`，因此这里不能像查询型 API 那样降级。
#[must_use]
pub fn build(
    tree: &mut NodeTree,
    parent: NodeId,
    spec: Arc<InputSpec>,
    theme: &Theme,
    state: &Signal<InputState>,
) -> InputNode {
    let tokens = theme.tokens();
    let size = tokens.for_size(spec.size);
    let border_width = tokens.border_width;
    let colors = theme.components().input;
    let danger_border = theme.role(Role::StatusDangerBorder);
    let disabled = spec.disabled;
    let masked = spec.masked;
    let multiline = spec.multiline;
    let name = spec.name.clone();
    let padding_x = size.padding_x;
    let padding_y = if multiline { size.padding_x } else { 0.0 };
    let background = if disabled {
        colors.background.disabled
    } else {
        colors.background.enabled
    };
    let foreground = if disabled {
        colors.foreground.disabled
    } else {
        colors.foreground.enabled
    };
    let normal_border = if disabled {
        colors.border.disabled
    } else {
        colors.border.normal
    };

    // ── 容器 ──────────────────────────────────────────────────────────────
    let mut root_style = StyleBuilder::new()
        .flex_row()
        .items_center()
        .relative()
        .width_full()
        .padding_x(padding_x)
        .padding_y(padding_y)
        .border(border_width)
        .overflow_hidden()
        .build();
    if multiline {
        // 多行：至少一个控件高，内容更高时随内容长高（不让窗口根把盒子拉满）。
        root_style.inner.min_size.height = LengthPercentageAuto::length(size.height);
        root_style.inner.align_self = Some(taffy::style::AlignSelf::START);
    } else {
        root_style.inner.size.height = taffy::style::Dimension::length(size.height);
    }
    let root_paint = PaintBuilder::new()
        .background(background)
        .border(border_width, normal_border)
        .rounded(CORNER_RADIUS)
        .build();

    let style_state = *state;
    let a11y_state = *state;
    let mut root_builder = NodeBuilder::boxed()
        .style(root_style)
        .paint(root_paint)
        .id(format_smolstr!("{name}.root"))
        .tab_stop(true)
        .a11y(A11yOverride {
            role: Some(if multiline {
                A11yRole::MultilineTextInput
            } else {
                A11yRole::TextInput
            }),
            label: Some(name.clone()),
            disabled,
            ..A11yOverride::default()
        })
        .bind_style(move |cx| {
            let state = cx.read(&style_state);
            let border_color = if disabled {
                colors.border.disabled
            } else if state.error().is_some() {
                danger_border
            } else if state.is_focused() {
                colors.border.focused
            } else {
                colors.border.normal
            };
            StylePatch {
                border: Some(Border {
                    width: border_width,
                    color: border_color,
                }),
                interactive: Some(!disabled),
                cursor: Some(if disabled {
                    Cursor::NotAllowed
                } else {
                    Cursor::Text
                }),
                ..StylePatch::default()
            }
        })
        .bind_a11y(move |cx| {
            let state = cx.read(&a11y_state);
            A11yPatch {
                value: Some(state.display_text(masked)),
                disabled: Some(disabled),
                ..A11yPatch::default()
            }
        });
    if !disabled {
        root_builder = root_builder.on_click(click_action(&spec));
    }
    let root = root_builder
        .build(tree, parent)
        .expect("input: root node mounts under a live parent");

    // ── 文本 ──────────────────────────────────────────────────────────────
    let text_state = *state;
    let text_style = TextStyle::new(size.font_size)
        .with_color(foreground)
        .with_wrap(if multiline {
            TextWrap::Wrap
        } else {
            TextWrap::NoWrap
        })
        .with_alignment(TextAlign::Start)
        .with_overflow(TextOverflow::Clip, 0);
    let text = NodeBuilder::text(SmolStr::new_static(""), text_style)
        .style(StyleBuilder::new().flex_1().min_width_0().build())
        .bind_text(move |cx| cx.read(&text_state).display_text(masked))
        .id(format_smolstr!("{name}.text"))
        .tab_stop(false)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, root)
        .expect("input: text node mounts under the fresh root");

    // ── 占位符（绝对覆盖在文本位上；空值可见）──────────────────────────────
    let placeholder_state = *state;
    let placeholder_text_style = TextStyle::new(size.font_size)
        .with_color(colors.placeholder)
        .with_wrap(if multiline {
            TextWrap::Wrap
        } else {
            TextWrap::NoWrap
        })
        .with_alignment(TextAlign::Start)
        .with_overflow(TextOverflow::Clip, 0);
    let placeholder = NodeBuilder::text(spec.placeholder.clone(), placeholder_text_style)
        .style(
            StyleBuilder::new()
                .absolute()
                .top(padding_y)
                .left(padding_x)
                .right(padding_x)
                .build(),
        )
        .bind_style(move |cx| {
            let state = cx.read(&placeholder_state);
            // 组合串可见时即使正文为空也压住占位符（否则 preedit 会叠在占位文字上）。
            // `interactive: false`：占位符与文本节点完全重叠，必须让命中穿透到文本节点。
            let empty = state.text().is_empty() && !state.is_composing();
            StylePatch {
                opacity: Some(if empty { 1.0 } else { 0.0 }),
                interactive: Some(false),
                ..StylePatch::default()
            }
        })
        .id(format_smolstr!("{name}.placeholder"))
        .tab_stop(false)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, root)
        .expect("input: placeholder node mounts under the fresh root");

    // ── caret / 选区 Canvas（绝对覆盖在文本位上；`sync` 换内容）─────────────
    let caret = NodeBuilder::canvas(Canvas::new(Rect::default()))
        .style(
            StyleBuilder::new()
                .absolute()
                .top(padding_y)
                .left(padding_x)
                .right(padding_x)
                .bottom(padding_y)
                .build(),
        )
        .bind_style(|_cx| StylePatch {
            interactive: Some(false),
            ..StylePatch::default()
        })
        .id(format_smolstr!("{name}.caret"))
        .tab_stop(false)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, root)
        .expect("input: caret canvas mounts under the fresh root");

    InputNode {
        root,
        text,
        placeholder,
        caret,
        state: *state,
        spec,
        caret_color: theme.role(Role::FgDefault),
        selection_color: colors.selection,
    }
}

impl InputNode {
    /// 挂载后调用一次：注册点击定位动作的处理器（焦点 + 光标位置）。
    ///
    pub fn install(&self, app: &mut App) {
        if self.spec.disabled {
            return;
        }
        let state = self.state;
        let spec = Arc::clone(&self.spec);
        let root = self.root;
        let text = self.text;
        app.on_action(click_action(&spec), move |ctx| {
            // 键盘激活（Enter/Space，`hit == None`）不是指针定位：不移动光标。
            let Some(hit) = ctx.hit() else {
                return;
            };
            let window = ctx.window();
            let extend = ctx.modifiers().contains(Modifiers::SHIFT);
            let app = ctx.app_mut();
            let Some(window_ref) = app.window(window) else {
                return;
            };
            let tree = window_ref.tree();
            let (Ok(hit_node), Ok(text_node)) = (tree.get(hit.node), tree.get(text)) else {
                return;
            };
            let hit_bounds = hit_node.layout().bounds;
            let text_bounds = text_node.layout().bounds;
            let local = Point::new(
                hit.local_point.x + hit_bounds.origin.x - text_bounds.origin.x,
                hit.local_point.y + hit_bounds.origin.y - text_bounds.origin.y,
            );
            if app.focused(window) != Some(root) {
                app.set_focus(window, Some(root));
            }
            let before = app.read(&state).clone();
            let mut next = before.clone();
            next.set_focused(true);
            let Some(window_ref) = app.window(window) else {
                return;
            };
            next.set_caret_from_point(window_ref, text, local, extend);
            if next != before {
                app.write(&state, next);
            }
            let _ = app.request_repaint(window);
        });
    }

    /// 每轮同步（外壳在帧之前调用）：焦点镜像、IME 开关与 caret 区域、caret/选区 Canvas。
    ///
    /// 返回是否写入了状态或换了画布。文本尚未在最近一帧 shape 时（刚编辑过）caret 几何要等
    /// 下一帧的 text pass，本方法保留旧画布并在下一轮再同步（最多落后一个泵轮）。
    pub fn sync(&self, app: &mut App) -> bool {
        let Some(window) = self.root.window() else {
            return false;
        };
        let Some(window_ref) = app.window(window) else {
            return false;
        };
        let focused = window_ref.focus().is_focused(self.root);
        let mut next = app.read(&self.state).clone();
        let mut state_changed = false;
        let was_focused = next.is_focused();
        if was_focused != focused {
            next.set_focused(focused);
            state_changed = true;
        }
        let ime_target = focused && !self.spec.disabled && !self.spec.readonly;
        if ime_target {
            app.set_ime_enabled(window, true);
            let byte = next.display_caret_byte(self.spec.masked);
            if app.set_ime_cursor_area(window, self.text, byte).is_err() {
                // 文本尚未 shape / 节点已卸载：平台会跳过本次 IME 区更新，下一轮再试。
            }
        } else {
            app.clear_ime_cursor_area(window);
            if was_focused {
                app.set_ime_enabled(window, false);
            }
        }
        let canvas = app
            .window(window)
            .and_then(|window_ref| self.caret_canvas(window_ref, &next));
        if state_changed {
            app.write(&self.state, next);
        }
        let canvas_changed = canvas.is_some_and(|canvas| self.apply_canvas(app, window, canvas));
        state_changed || canvas_changed
    }

    /// 事件路由（外壳 `drain_events` 之后逐条调用）。返回是否消费。
    ///
    /// 只处理聚焦在本输入框上的 `Text` / `Key(Down)` / `Ime`；其余事件（Tab、Esc 关浮层、
    /// 指针……）留给外壳与引擎。
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let Some(window) = self.root.window() else {
            return false;
        };
        if app.focused(window) != Some(self.root) {
            return false;
        }
        let before = app.read(&self.state).clone();
        let mut next = before.clone();
        let consumed = match event {
            Event::Input(InputEvent::Text { text, .. }) => {
                if event::should_emit_text(KeyState::Down, false, false, next.is_composing()) {
                    next.insert(&self.spec, text);
                }
                true
            }
            Event::Input(InputEvent::Key {
                code,
                state: KeyState::Down,
                repeat,
                modifiers,
                ..
            }) => {
                if *repeat {
                    false
                } else {
                    next.set_focused(true);
                    next.handle_key(app, window, &self.spec, *code, *modifiers)
                }
            }
            Event::Input(InputEvent::Key { .. }) => false,
            Event::Ime(ime) => {
                next.set_focused(true);
                next.handle_ime(app, window, &self.spec, ime)
            }
            _ => false,
        };
        if consumed {
            // `handle_key` 可能派发了 submit 动作，而它的处理器可能已经写过同一个信号
            // （例如提交后清空输入）：以处理器写入的值为准，不覆盖。
            let current = app.read(&self.state).clone();
            if current == before && next != before {
                app.write(&self.state, next);
            }
        }
        consumed
    }

    /// 构造当前帧的 caret + 选区画布（`None` = 文本尚未 shape，保留旧画布）。
    fn caret_canvas(&self, window: &Window, state: &InputState) -> Option<Canvas> {
        if !window.focus().is_focused(self.root) {
            return Some(Canvas::new(Rect::default()));
        }
        let masked = self.spec.masked;
        let caret_byte = state.display_caret_byte(masked);
        let caret_rect = window.text_caret_rect(self.text, caret_byte).ok()?;
        // 选区矩形是文本盒局部坐标；`Window::text_caret_rect` 是窗口绝对坐标。两者在字节 0
        // 处的差就是「节点原点 + 内容偏移 + 祖先滚动偏移」。
        let local_zero = window.tree().text_caret_rect(self.text, 0).ok()?;
        let absolute_zero = window.text_caret_rect(self.text, 0).ok()?;
        let origin = Point::new(
            absolute_zero.origin.x - local_zero.origin.x,
            absolute_zero.origin.y - local_zero.origin.y,
        );
        let selection = state.display_selection(masked);
        let mut quads: Vec<Quad> = Vec::with_capacity(4);
        if !selection.is_empty() {
            let rects = window
                .tree()
                .text_selection_rects(self.text, selection.start, selection.end)
                .ok()?;
            for rect in rects {
                quads.push(Quad {
                    bounds: rect.translate(origin),
                    corner_radius: [0.0; 4],
                    paint: QuadPaint::Solid {
                        fill: self.selection_color,
                        border: None,
                    },
                });
            }
        }
        quads.push(Quad {
            bounds: caret_rect,
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: self.caret_color,
                border: None,
            },
        });
        let bounds = quads
            .iter()
            .fold(Rect::default(), |bounds, quad| bounds.union(quad.bounds));
        let mut canvas = Canvas::new(bounds);
        for quad in quads {
            canvas.push_quad(quad);
        }
        Some(canvas)
    }

    /// 把画布交给引擎（同值跳过；变化时请求一帧）。
    fn apply_canvas(&self, app: &mut App, window: WindowId, canvas: Canvas) -> bool {
        let unchanged = app.window(window).is_some_and(|window_ref| {
            match window_ref.tree().get(self.caret) {
                Ok(node) => {
                    matches!(node.kind(), NodeKind::Canvas(existing) if existing.as_ref() == &canvas)
                }
                Err(_) => false,
            }
        });
        if unchanged {
            return false;
        }
        let applied = app
            .window_mut(window)
            .is_some_and(|window_ref| window_ref.set_canvas(self.caret, canvas).unwrap_or(false));
        if applied {
            let _ = app.request_repaint(window);
        }
        applied
    }
}

/// 点击定位动作的 key（`spec.name` 是实例唯一性来源）。
fn click_action(spec: &InputSpec) -> Action {
    Action::Custom(format_smolstr!("base_ui.input.{}.click", spec.name))
}

/// 未脱敏显示文本字节 → 脱敏显示文本字节（每个字符一个 `•`）。
fn masked_offset(unmasked_display: &str, byte: usize) -> usize {
    let byte = snap_char_boundary(unmasked_display, byte);
    unmasked_display[..byte].chars().count() * BULLET.len_utf8()
}

/// 把字节偏移吸附到 `char` 边界（向下取整，clamp 到文本长度）。
fn snap_char_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

/// 前一个 `char` 的起点（`byte` 已在 char boundary）。
fn prev_char_boundary(text: &str, byte: usize) -> Option<usize> {
    text[..byte]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
}

/// 后一个 `char` 的起点（`byte` 已在 char boundary）。
fn next_char_boundary(text: &str, byte: usize) -> Option<usize> {
    text[byte..].chars().next().map(|ch| byte + ch.len_utf8())
}

/// 第 `char_index` 个字符的字节偏移（越过末尾时返回文本长度）。
fn byte_at_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// UTF-16 码元数（代理对算 2）。
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// UTF-16 码元偏移 → UTF-8 字节偏移（clamp + 向下吸附到 `char` 边界）。
fn utf8_from_utf16_offset(text: &str, offset: u16) -> usize {
    utf8_from_utf16_units(text, usize::from(offset))
}

/// [`utf8_from_utf16_offset`] 的 `usize` 版本（IME 替换区间是 `Range<usize>`）。
fn utf8_from_utf16_units(text: &str, offset: usize) -> usize {
    let mut remaining = offset;
    for (byte, ch) in text.char_indices() {
        if remaining == 0 {
            return byte;
        }
        let units = ch.len_utf16();
        if remaining < units {
            return byte;
        }
        remaining -= units;
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

    use smol_str::SmolStr;
    use ui_core::{
        event::{Action, Event, ImeEvent, InputEvent, KeyCode, KeyState, Modifiers, PointerButton},
        math::{Point, Rect},
        node::{id::NodeId, signal::Signal, tree::NodeKind},
        paint::primitive::Primitive,
        test_support::{app::TestApp, record::PrimitiveKind},
        window::WindowId,
    };

    use super::{
        CaretMotion, InputError, InputNode, InputSpec, InputState, build, click_action,
        utf8_from_utf16_offset, utf16_len,
    };
    use crate::theme::{components::ControlSize, resolved::Theme};

    // ── 构造工具 ────────────────────────────────────────────────────────────

    fn spec() -> InputSpec {
        InputSpec {
            name: SmolStr::new_static("test"),
            placeholder: SmolStr::new_static("请输入"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: ControlSize::Medium,
            submit: None,
        }
    }

    fn state(text: &str) -> InputState {
        InputState::new(text)
    }

    fn focused_state(text: &str) -> InputState {
        let mut state = InputState::new(text);
        state.set_focused(true);
        state
    }

    struct Harness {
        app: TestApp,
        node: InputNode,
        state: Signal<InputState>,
    }

    fn harness(spec: InputSpec, initial: InputState) -> Harness {
        let theme = Theme::default();
        let mut app = TestApp::new();
        let state = app.app_mut().signal(initial);
        let spec = Arc::new(spec);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let node = build(
            app.app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut(),
            root,
            Arc::clone(&spec),
            &theme,
            &state,
        );
        node.install(app.app_mut());
        app.drain().expect("首帧");
        Harness { app, node, state }
    }

    impl Harness {
        fn window(&self) -> WindowId {
            self.app.window_id()
        }

        fn drain(&mut self) {
            self.app.drain().expect("帧");
        }

        fn focus(&mut self) {
            let window = self.window();
            let root = self.node.root;
            assert!(
                self.app.app_mut().set_focus(window, Some(root)),
                "根节点可聚焦"
            );
            let _ = self.node.sync(self.app.app_mut());
            self.drain();
        }

        fn sync(&mut self) {
            let _ = self.node.sync(self.app.app_mut());
        }

        fn pump(&mut self, event: Event) {
            self.app.dispatch_event(event);
            let window = self.window();
            let events = self.app.app_mut().drain_events();
            for (target, event) in events {
                assert_eq!(target, window);
                self.node.dispatch(self.app.app_mut(), &event);
            }
        }

        fn type_text(&mut self, text: &str) {
            self.pump(Event::Input(InputEvent::Text {
                text: SmolStr::new(text),
                timestamp: Instant::now(),
            }));
        }

        fn key(&mut self, code: KeyCode, modifiers: Modifiers) {
            self.pump(Event::Input(InputEvent::Key {
                code,
                state: KeyState::Down,
                repeat: false,
                modifiers,
                timestamp: Instant::now(),
            }));
        }

        fn ime(&mut self, event: ImeEvent) {
            self.pump(Event::Ime(event));
        }

        fn state(&self) -> InputState {
            self.app.app().read(&self.state).clone()
        }

        fn node_text(&self, node: NodeId) -> SmolStr {
            let window = self.window();
            let tree = self.app.app().window(window).expect("窗口存在").tree();
            match tree.get(node).expect("节点存在").kind() {
                NodeKind::Text(content) => content.text.clone(),
                other => panic!("expected text node, got {other:?}"),
            }
        }

        fn root_quad_count(&self) -> usize {
            let Some(frame) = self.app.frame_record().frame.as_ref() else {
                return 0;
            };
            frame
                .quads
                .iter()
                .filter(|quad| quad.node == self.node.root)
                .count()
        }

        fn caret_quads(&self) -> Vec<Rect> {
            let window = self.window();
            let tree = self.app.app().window(window).expect("窗口存在").tree();
            let NodeKind::Canvas(canvas) = tree.get(self.node.caret).expect("节点存在").kind()
            else {
                panic!("caret 节点必须是 Canvas");
            };
            canvas
                .primitives()
                .iter()
                .filter_map(|primitive| match primitive {
                    Primitive::Quad(quad) => Some(quad.bounds),
                    _ => None,
                })
                .collect()
        }

        fn window_ref(&self) -> &ui_core::window::Window {
            self.app.app().window(self.window()).expect("窗口存在")
        }
    }

    // ── 纯状态机：基本编辑 ────────────────────────────────────────────────

    #[test]
    fn insert_delete_backward_and_forward_edit_ascii() {
        let spec = spec();
        let mut state = state("");
        assert!(state.insert(&spec, "abc"));
        assert_eq!(state.text(), "abc");
        assert_eq!(state.caret(), 3);
        assert!(state.delete_backward(&spec));
        assert_eq!(state.text(), "ab");
        assert_eq!(state.caret(), 2);
        state.set_caret(0, false);
        assert!(state.delete_forward(&spec));
        assert_eq!(state.text(), "b");
        assert_eq!(state.caret(), 0);
        assert!(!state.delete_backward(&spec), "行首没有可删字符");
        assert!(state.delete_forward(&spec), "光标后仍有字符");
        assert_eq!(state.text(), "");
        assert!(!state.delete_forward(&spec), "空文本没有可删字符");
    }

    #[test]
    fn insert_replaces_the_selection() {
        let spec = spec();
        let mut state = state("hello");
        state.set_caret(1, false);
        state.set_caret(4, true);
        assert_eq!(state.selection(), 1..4);
        assert!(state.insert(&spec, "i"));
        assert_eq!(state.text(), "hio");
        assert_eq!(state.caret(), 2);
        assert_eq!(state.selection(), 2..2);
    }

    #[test]
    fn multi_byte_cjk_edits_never_split_bytes() {
        let spec = spec();
        let mut state = state("你好世界");
        assert_eq!(state.caret(), 12);
        assert!(state.delete_backward(&spec));
        assert_eq!(state.text(), "你好世");
        assert_eq!(state.caret(), 9);
        assert!(state.text().is_char_boundary(state.caret()));
        state.set_caret(6, false);
        assert!(state.delete_backward(&spec));
        assert_eq!(state.text(), "你世");
        state.set_caret(0, false);
        assert!(state.delete_forward(&spec));
        assert_eq!(state.text(), "世");
        assert!(state.text().is_char_boundary(state.caret()));
    }

    #[test]
    fn set_caret_snaps_inside_emoji_and_clamps() {
        let mut state = state("a🌍b");
        state.set_caret(2, false);
        assert_eq!(state.caret(), 1, "落在 emoji 中间必须向下吸附到 char 起点");
        state.set_caret(999, false);
        assert_eq!(state.caret(), "a🌍b".len());
        state.set_caret(9, false);
        assert!(state.text().is_char_boundary(state.caret()));
    }

    #[test]
    fn arrows_and_shift_select_characters() {
        let mut state = focused_state("ab中");
        state.set_caret(0, false);
        state.move_caret(CaretMotion::CharRight, false);
        assert_eq!(state.caret(), 1);
        state.move_caret(CaretMotion::CharRight, false);
        assert_eq!(state.caret(), 2);
        state.move_caret(CaretMotion::CharRight, true);
        assert_eq!(state.selection(), 2..5, "Shift 扩选一个 CJK 字符");
        state.move_caret(CaretMotion::CharLeft, false);
        assert_eq!(state.selection(), 2..2, "无 Shift 取消锚点");
        assert_eq!(state.caret(), 2);
        state.move_caret(CaretMotion::DocEnd, false);
        assert_eq!(state.caret(), 5);
        state.move_caret(CaretMotion::CharLeft, false);
        assert_eq!(state.caret(), 2);
    }

    #[test]
    fn home_and_end_stay_on_the_current_line() {
        let mut state = state("one\ntwo\nthree");
        state.set_caret(6, false);
        state.move_caret(CaretMotion::Home, false);
        assert_eq!(state.caret(), 4);
        state.move_caret(CaretMotion::End, false);
        assert_eq!(state.caret(), 7);
        state.move_caret(CaretMotion::Home, true);
        assert_eq!(state.selection(), 4..7);
        state.move_caret(CaretMotion::DocStart, false);
        assert_eq!(state.caret(), 0);
        state.move_caret(CaretMotion::DocEnd, false);
        assert_eq!(state.caret(), 13);
    }

    #[test]
    fn select_all_and_replace() {
        let spec = spec();
        let mut state = state("你好");
        state.select_all();
        assert_eq!(state.selection(), 0..6);
        assert!(state.insert(&spec, "hi"));
        assert_eq!(state.text(), "hi");
        assert_eq!(state.caret(), 2);
    }

    #[test]
    fn insert_newline_is_multiline_only() {
        let mut single = focused_state("");
        assert!(!single.insert_newline(&spec()));
        let multiline = InputSpec {
            multiline: true,
            ..spec()
        };
        let mut multi = focused_state("");
        assert!(multi.insert_newline(&multiline));
        assert_eq!(multi.text(), "\n");
        assert_eq!(multi.caret(), 1);
    }

    #[test]
    fn max_length_clips_ascii_and_cjk_by_chars() {
        let ascii_limit = InputSpec {
            max_length: Some(3),
            ..spec()
        };
        let mut ascii = state("");
        assert!(ascii.insert(&ascii_limit, "abcdef"));
        assert_eq!(ascii.text(), "abc");
        assert!(
            !ascii.insert(&ascii_limit, "汉字"),
            "已满且无选区时插入是 no-op"
        );
        assert_eq!(ascii.text(), "abc");

        let cjk_limit = InputSpec {
            max_length: Some(2),
            ..spec()
        };
        let mut cjk = state("");
        assert!(cjk.insert(&cjk_limit, "你好世界"));
        assert_eq!(cjk.text(), "你好");
        // 选区替换也按剩余预算裁剪，且永不切开字符。
        cjk.select_all();
        let smaller = InputSpec {
            max_length: Some(1),
            ..spec()
        };
        assert!(cjk.insert(&smaller, "🌍🌍"));
        assert_eq!(cjk.text(), "🌍");
        assert!(cjk.text().is_char_boundary(cjk.text().len()));
    }

    #[test]
    fn masked_display_uses_one_bullet_per_character() {
        let mut state = state("");
        state.insert(&spec(), "a你🌍");
        assert_eq!(state.display_text(true), "•••");
        assert_eq!(state.display_text(true).chars().count(), 3);
        assert_eq!(state.display_text(false), "a你🌍");
        assert_eq!(state.caret(), 1 + 3 + 4);
        assert_eq!(
            state.display_caret_byte(true),
            '•'.len_utf8() * 3,
            "脱敏 caret 按字符数换算"
        );
    }

    #[test]
    fn readonly_rejects_every_mutation() {
        let spec = InputSpec {
            readonly: true,
            ..spec()
        };
        let mut state = state("abc");
        assert!(!state.insert(&spec, "x"));
        assert!(!state.delete_backward(&spec));
        assert!(!state.delete_forward(&spec));
        assert!(!state.insert_newline(&spec));
        assert!(!state.ime_preedit(&spec, "ni", None));
        assert!(!state.ime_commit(&spec, "ni"));
        assert_eq!(state.text(), "abc");
        // 选区与光标仍然可用。
        state.select_all();
        assert_eq!(state.selection(), 0..3);
    }

    #[test]
    fn disabled_rejects_edits() {
        let spec = InputSpec {
            disabled: true,
            ..spec()
        };
        let mut state = state("abc");
        assert!(!state.insert(&spec, "x"));
        assert!(!state.delete_backward(&spec));
        assert_eq!(state.text(), "abc");
    }

    #[test]
    fn validate_reports_empty_and_too_long_with_stable_i18n_keys() {
        let mut empty_state = state("");
        assert_eq!(empty_state.validate(&spec()), Some(InputError::Empty));
        assert_eq!(
            empty_state.error().map(InputError::i18n_key),
            Some("input.error.empty")
        );
        let limit = InputSpec {
            max_length: Some(1),
            ..spec()
        };
        let mut too_long = state("ab");
        assert_eq!(
            too_long.validate(&limit),
            Some(InputError::TooLong { max: 1 })
        );
        assert_eq!(
            too_long.error().map(InputError::i18n_key),
            Some("input.error.tooLong")
        );
        too_long.set_error(Some(InputError::Invalid {
            i18n_key: SmolStr::new_static("account.error.nameTaken"),
        }));
        assert_eq!(
            too_long.error().map(InputError::i18n_key),
            Some("account.error.nameTaken")
        );
    }

    // ── UTF-16 与 IME ──────────────────────────────────────────────────────

    #[test]
    fn utf16_helpers_count_surrogate_pairs_and_snap() {
        let text = "a🌍中";
        assert_eq!(utf16_len(text), 4, "ASCII 1 + emoji 2 + CJK 1");
        assert_eq!(utf8_from_utf16_offset(text, 0), 0);
        assert_eq!(utf8_from_utf16_offset(text, 1), 1);
        assert_eq!(
            utf8_from_utf16_offset(text, 2),
            1,
            "落在代理对内部：向下吸附到 emoji 起点"
        );
        assert_eq!(utf8_from_utf16_offset(text, 3), 5);
        assert_eq!(utf8_from_utf16_offset(text, 4), 8);
        assert_eq!(utf8_from_utf16_offset(text, 99), 8, "越界 clamp");
    }

    #[test]
    fn ime_preedit_then_commit_cjk() {
        let mut state = focused_state("");
        assert!(state.ime_preedit(&spec(), "ni", Some(2)));
        assert!(state.is_composing());
        assert_eq!(state.text(), "", "preedit 不进入提交文本");
        assert_eq!(state.display_text(false), "ni");
        assert_eq!(state.caret(), 2);
        assert!(state.ime_commit(&spec(), "你"));
        assert!(!state.is_composing());
        assert_eq!(state.text(), "你");
        assert_eq!(state.caret(), 3);
        assert_eq!(state.display_text(false), "你");
    }

    #[test]
    fn ime_commit_without_preedit_inserts_at_caret() {
        let mut state = focused_state("ab");
        assert!(state.ime_commit(&spec(), "中"));
        assert_eq!(state.text(), "ab中");
        assert_eq!(state.caret(), 5);
        assert!(!state.is_composing());
    }

    #[test]
    fn ime_cancel_on_end_drops_the_preedit() {
        let mut state = focused_state("ab");
        assert!(state.ime_preedit(&spec(), "ni", None));
        assert_eq!(state.display_text(false), "abni");
        state.ime_cancel();
        assert!(!state.is_composing());
        assert_eq!(state.display_text(false), "ab");
        assert_eq!(state.caret(), 2);
        assert_eq!(state.text(), "ab");
    }

    #[test]
    fn ime_preedit_replace_subrange_keeps_the_untouched_text() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let spec = spec();
        let mut state = focused_state("");
        assert!(state.ime_preedit(&spec, "abcdef", Some(6)));
        assert!(state.handle_ime(
            app.app_mut(),
            window,
            &spec,
            &ImeEvent::Preedit {
                text: SmolStr::new_static("XY"),
                caret: Some(2),
                clauses: Default::default(),
                replace: Some(2..4),
            },
        ));
        assert_eq!(state.display_text(false), "abXYef");
        assert!(state.ime_commit(&spec, "abXYef"));
        assert_eq!(state.text(), "abXYef");
    }

    #[test]
    fn ime_caret_utf16_is_relative_to_the_preedit_not_the_document() {
        // 组合串前面有 emoji：caret 的 UTF-16 偏移只数组合串内部。把 caret 当成整串偏移的
        // 实现会在这里多算 2 个码元（本用例是该实现的变异守卫）。
        let mut state = focused_state("🌍ab");
        assert_eq!(state.caret(), 6);
        assert!(state.ime_preedit(&spec(), "你好", Some(1)));
        assert_eq!(
            state.caret(),
            6 + 3,
            "组合串内 caret=1 → 第一个汉字之后（不是整串 UTF-16 偏移）"
        );
        assert_eq!(state.display_text(false), "🌍ab你好");
        assert!(state.ime_preedit(&spec(), "你好", Some(2)));
        assert_eq!(state.caret(), 6 + 6);
        // 组合串内再来一个代理对：caret=1 落在 emoji 内部 ⇒ 吸附到 emoji 起点；caret=2 是
        // emoji 之后。
        assert!(state.ime_preedit(&spec(), "🌍a", Some(2)));
        assert_eq!(
            state.caret(),
            6 + 4,
            "emoji 的 2 个 UTF-16 码元 = 4 个 UTF-8 字节"
        );
        assert!(state.ime_preedit(&spec(), "🌍a", Some(1)));
        assert_eq!(state.caret(), 6, "落在代理对内部：向下吸附到 emoji 起点");
        assert!(state.ime_commit(&spec(), "🌍a"));
        assert_eq!(state.text(), "🌍ab🌍a");
    }

    #[test]
    fn ime_start_clears_the_selection() {
        let mut state = focused_state("hello");
        state.set_caret(1, false);
        state.set_caret(3, true);
        assert!(state.ime_preedit(&spec(), "ni", Some(1)));
        assert_eq!(state.text(), "hlo", "组合开始时选区被删掉");
        assert_eq!(state.display_text(false), "hnilo");
        assert!(state.ime_commit(&spec(), "你"));
        assert_eq!(state.text(), "h你lo");
    }

    #[test]
    fn losing_focus_drops_the_preedit() {
        let mut state = focused_state("ab");
        assert!(state.ime_preedit(&spec(), "ni", None));
        assert!(state.is_composing());
        state.set_focused(false);
        assert!(!state.is_composing());
        assert_eq!(state.text(), "ab");
    }

    // ── 剪贴板 ──────────────────────────────────────────────────────────────

    #[test]
    fn clipboard_copy_cut_paste_through_app() {
        let app = TestApp::new();
        let app_ref = app.app();
        // headless 测试引擎没有原生窗口 owner：平台会拒绝剪贴板。此时只断言「不 panic 且返回
        // false」；真正接上原生窗口的宿主会走正向分支。
        let platform = app_ref.set_clipboard_text("seed").is_ok();
        let mut state = state("hello");
        state.set_caret(1, false);
        state.set_caret(4, true);
        let copied = state.copy(app_ref, &spec());
        if platform {
            assert!(copied, "有原生剪贴板时复制必须成功");
            assert_eq!(app_ref.clipboard_text().expect("读回"), "ell");
            let mut target = self::state("");
            assert!(target.paste(app_ref, &spec()));
            assert_eq!(target.text(), "ell");
        } else {
            assert!(!copied, "平台拒绝剪贴板时复制返回 false");
            assert!(!state.paste(app_ref, &spec()));
        }
        // 掩码字段永不走剪贴板；只读字段不许剪切/粘贴。
        let masked = InputSpec {
            masked: true,
            ..spec()
        };
        assert!(!state.copy(app_ref, &masked));
        assert!(!state.cut(app_ref, &masked));
        let readonly = InputSpec {
            readonly: true,
            ..spec()
        };
        assert!(!state.cut(app_ref, &readonly));
        assert!(!state.paste(app_ref, &readonly));
        // 无选区 ⇒ false（与平台可用性无关）。
        let empty = self::state("");
        assert!(!empty.copy(app_ref, &spec()));
    }

    // ── 引擎集成 ────────────────────────────────────────────────────────────

    #[test]
    fn build_renders_background_and_text_and_placeholder() {
        let harness = harness(spec(), state("hello"));
        assert!(harness.root_quad_count() >= 1, "根节点必须画出背景");
        assert_eq!(harness.node_text(harness.node.text), "hello");
        assert_eq!(harness.node_text(harness.node.placeholder), "请输入");
    }

    #[test]
    fn typing_through_dispatch_grows_the_text_node() {
        let mut harness = harness(spec(), state(""));
        harness.focus();
        harness.type_text("你");
        harness.type_text("a");
        harness.drain();
        assert_eq!(harness.state().text(), "你a");
        assert_eq!(harness.node_text(harness.node.text), "你a");
    }

    #[test]
    fn caret_canvas_tracks_window_caret_rect() {
        let mut harness = harness(spec(), state("hi"));
        harness.focus();
        harness.sync();
        harness.drain();
        let caret_rect = harness
            .window_ref()
            .text_caret_rect(harness.node.text, 2)
            .expect("文本已 shape");
        let caret_quad = harness.caret_quads().last().copied().expect("caret quad");
        assert!(
            (caret_quad.origin.x - caret_rect.origin.x).abs() <= 0.5
                && (caret_quad.origin.y - caret_rect.origin.y).abs() <= 0.5,
            "caret 画布必须与引擎 caret 矩形同源：{caret_quad:?} vs {caret_rect:?}"
        );
        assert!((caret_quad.size.width - caret_rect.size.width).abs() <= 0.5);
    }

    #[test]
    fn masked_caret_canvas_uses_bullet_offsets() {
        let mut harness = harness(
            InputSpec {
                masked: true,
                ..spec()
            },
            state("ab"),
        );
        harness.focus();
        // 脱敏显示是「••」，caret 在末尾 = 2 个 `•` 的字节偏移（6），不是真实文本的 2。
        let bullet_caret = 2 * '•'.len_utf8();
        let caret_rect = harness
            .window_ref()
            .text_caret_rect(harness.node.text, bullet_caret)
            .expect("脱敏文本已 shape");
        let caret_quad = harness.caret_quads().last().copied().expect("caret quad");
        assert!((caret_quad.origin.x - caret_rect.origin.x).abs() <= 0.5);
        assert!((caret_quad.origin.y - caret_rect.origin.y).abs() <= 0.5);
        let area = harness
            .app
            .app()
            .ime_cursor_area(harness.window())
            .expect("窗口存在")
            .expect("聚焦时必须登记 IME caret 区域");
        assert!((area.origin.x - caret_rect.origin.x).abs() <= 0.5);
    }

    #[test]
    fn selection_draws_quads_behind_the_caret() {
        let mut harness = harness(spec(), state("hello"));
        harness.focus();
        harness
            .app
            .app_mut()
            .update(&harness.state, |state| state.select_all());
        harness.sync();
        harness.drain();
        let quads = harness.caret_quads();
        assert!(quads.len() >= 2, "全选必须画出选区矩形 + caret：{quads:?}");
    }

    #[test]
    fn sync_mirrors_focus_and_sets_ime_cursor_area() {
        let mut harness = harness(spec(), state("hi"));
        let window = harness.window();
        let root = harness.node.root;
        harness.app.app_mut().set_focus(window, Some(root));
        assert!(harness.node.sync(harness.app.app_mut()));
        harness.drain();
        assert!(harness.state().is_focused(), "焦点镜像必须写回状态");
        let area = harness
            .app
            .app()
            .ime_cursor_area(window)
            .expect("窗口存在")
            .expect("聚焦时必须登记 IME caret 区域");
        let caret_rect = harness
            .window_ref()
            .text_caret_rect(harness.node.text, 2)
            .expect("文本已 shape");
        assert!((area.origin.x - caret_rect.origin.x).abs() <= 0.5);
        assert!((area.origin.y - caret_rect.origin.y).abs() <= 0.5);
        // 失焦：IME 区域清空、焦点镜像归 false。
        harness.app.app_mut().set_focus(window, None);
        assert!(harness.node.sync(harness.app.app_mut()));
        assert!(!harness.state().is_focused());
        assert!(
            harness
                .app
                .app()
                .ime_cursor_area(window)
                .expect("窗口存在")
                .is_none()
        );
    }

    #[test]
    fn set_caret_from_point_uses_shaped_text_and_char_boundaries() {
        let harness = harness(spec(), state("ab"));
        let window = harness.window();
        let bounds = harness
            .app
            .probe()
            .bounds(harness.node.text)
            .expect("已布局");
        let window_ref = harness.window_ref();
        let mut state = harness.state();
        assert!(state.set_caret_from_point(
            window_ref,
            harness.node.text,
            Point::new(1.0, bounds.size.height / 2.0),
            false
        ));
        assert_eq!(state.caret(), 0, "点在最左：字节 0");
        assert!(state.set_caret_from_point(
            window_ref,
            harness.node.text,
            Point::new(bounds.size.width - 0.5, bounds.size.height / 2.0),
            false
        ));
        assert_eq!(state.caret(), 2, "点在末尾：文本长度");
        assert!(state.text().is_char_boundary(state.caret()));
        // 非文本节点：返回 false 而不是猜测。
        assert!(!state.set_caret_from_point(window_ref, harness.node.root, Point::ZERO, false));
        let _ = window;
    }

    #[test]
    fn click_action_moves_caret_and_focuses_the_input() {
        let mut harness = harness(spec(), state("hello"));
        let bounds = harness
            .app
            .probe()
            .bounds(harness.node.text)
            .expect("已布局");
        let position = Point::new(
            bounds.origin.x + 1.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        harness.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.drain();
        assert_eq!(
            harness.app.app().focused(harness.window()),
            Some(harness.node.root),
            "主键点击必须把焦点交给输入框"
        );
        assert_eq!(harness.state().caret(), 0, "点在文本最左：caret 到 0");
    }

    #[test]
    fn disabled_input_is_not_focusable_and_ignores_edits() {
        let mut harness = harness(
            InputSpec {
                disabled: true,
                ..spec()
            },
            state("hello"),
        );
        let bounds = harness
            .app
            .probe()
            .bounds(harness.node.root)
            .expect("已布局");
        let position = Point::new(bounds.origin.x + 2.0, bounds.origin.y + 2.0);
        harness.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.drain();
        assert_eq!(
            harness.app.app().focused(harness.window()),
            None,
            "禁用输入框不可聚焦"
        );
        let window = harness.window();
        let tree = harness.app.app().window(window).expect("窗口存在").tree();
        assert_eq!(
            tree.get(harness.node.root)
                .expect("节点存在")
                .style_patch()
                .interactive,
            Some(false),
            "禁用态必须通过 StylePatch.interactive 关闭交互"
        );
    }

    #[test]
    fn masked_text_is_rendered_as_bullets() {
        let mut harness = harness(
            InputSpec {
                masked: true,
                ..spec()
            },
            state(""),
        );
        harness.focus();
        harness.type_text("你a");
        harness.drain();
        assert_eq!(harness.state().text(), "你a");
        assert_eq!(harness.node_text(harness.node.text), "••");
    }

    #[test]
    fn ime_events_route_through_dispatch_into_the_text_node() {
        let mut harness = harness(spec(), state(""));
        harness.focus();
        harness.ime(ImeEvent::Preedit {
            text: SmolStr::new_static("你好"),
            caret: Some(1),
            clauses: Default::default(),
            replace: None,
        });
        harness.drain();
        assert_eq!(harness.node_text(harness.node.text), "你好");
        assert!(harness.state().is_composing());
        harness.ime(ImeEvent::Commit {
            text: SmolStr::new_static("你好"),
            replace: None,
        });
        harness.drain();
        assert_eq!(harness.state().text(), "你好");
        assert!(!harness.state().is_composing());
        assert_eq!(harness.node_text(harness.node.text), "你好");
    }

    #[test]
    fn key_events_route_through_dispatch_and_enter_submits() {
        let submit = Action::custom("base_ui.input.submit");
        let mut harness = harness(
            InputSpec {
                submit: Some(submit.clone()),
                ..spec()
            },
            state(""),
        );
        let hits = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let counter = hits.clone();
        harness.app.app_mut().on_action(submit, move |_ctx| {
            counter.set(counter.get() + 1);
        });
        harness.focus();
        harness.type_text("ab");
        harness.key(KeyCode::ArrowLeft, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().caret(), 1);
        harness.key(KeyCode::Enter, Modifiers::empty());
        assert_eq!(hits.get(), 1, "Enter 必须派发 spec.submit");
        harness.key(KeyCode::Backspace, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().text(), "b");
    }

    #[test]
    fn multiline_enter_inserts_a_newline_through_dispatch() {
        let mut harness = harness(
            InputSpec {
                multiline: true,
                ..spec()
            },
            state(""),
        );
        harness.focus();
        harness.type_text("ab");
        harness.key(KeyCode::Enter, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().text(), "ab\n");
        assert_eq!(harness.node_text(harness.node.text), "ab\n");
    }

    #[test]
    fn caret_canvas_primitive_is_emitted_for_the_caret_node() {
        let mut harness = harness(spec(), state("hi"));
        harness.focus();
        let record = harness.app.frame_record();
        let frame = record.frame.as_ref().expect("提交帧");
        assert!(
            frame.primitives.iter().any(|primitive| {
                primitive.node == harness.node.caret && primitive.kind == PrimitiveKind::Canvas
            }),
            "caret Canvas 节点必须进入绘制序列"
        );
    }

    #[test]
    fn click_action_key_is_namespaced_by_spec_name() {
        let spec = InputSpec {
            name: SmolStr::new_static("account.email"),
            ..spec()
        };
        assert_eq!(
            format!("{:?}", click_action(&spec)),
            "Custom(\"base_ui.input.account.email.click\")"
        );
    }

    #[test]
    fn readonly_input_still_gets_a_caret_canvas() {
        let mut harness = harness(
            InputSpec {
                readonly: true,
                ..spec()
            },
            state("abc"),
        );
        harness.focus();
        harness.sync();
        harness.drain();
        assert!(!harness.caret_quads().is_empty(), "只读输入仍要画 caret");
        assert!(
            harness
                .app
                .app()
                .ime_cursor_area(harness.window())
                .expect("窗口存在")
                .is_none(),
            "只读输入不登记 IME caret 区域"
        );
    }

    #[test]
    fn error_state_changes_the_border_patch() {
        let mut harness = harness(spec(), state("abc"));
        let window = harness.window();
        // 初始（未聚焦/无错误）：normal 边框色。
        let normal = {
            let tree = harness.app.app().window(window).expect("窗口存在").tree();
            tree.get(harness.node.root)
                .expect("节点存在")
                .style_patch()
                .border
                .expect("绑定边框")
        };
        harness.app.app_mut().update(&harness.state, |state| {
            state.set_error(Some(InputError::Invalid {
                i18n_key: SmolStr::new_static("account.error.nameTaken"),
            }));
        });
        harness.drain();
        let with_error = {
            let tree = harness.app.app().window(window).expect("窗口存在").tree();
            tree.get(harness.node.root)
                .expect("节点存在")
                .style_patch()
                .border
                .expect("绑定边框")
        };
        assert_ne!(
            normal.color, with_error.color,
            "错误态必须换掉边框颜色（StatusDangerBorder）"
        );
    }
}

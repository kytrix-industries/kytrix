//! IME 通路（IMM32）：组合串（文本 + 插入点 + 子句）、提交去重、caret 表单、启停上下文。
//!
//! 事件通路负责组合开始/更新/结束与 `GCS_RESULTSTR` 提交；caret 区域经
//! `ImmSetCompositionWindow` + `ImmSetCandidateWindow` 定位（不再使用最后指针位置）。
//!
//! ## 三条纪律
//!
//! 1. **不在持有 `&mut WndProcState` 时调用会重入的 IMM32 API**（R07-7）。
//!    `ImmAssociateContext*` / `ImmSetCompositionWindow` / `ImmSetCandidateWindow` 都会
//!    **同步**派发 `WM_IME_SETCONTEXT` / `WM_IME_NOTIFY`，嵌套进入的 `WndProc` 会再取一次
//!    `GWLP_USERDATA` 并构造 `&mut WndProcState` —— 同一对象上两个活跃 `&mut` 是别名 UB。
//!    所以会重入的调用全部包成 [`ImeCommand`]（纯数据）：调用方先在短借用下算出命令
//!    （[`ImeState::plan_enabled`] 等），**释放借用**，再 [`execute`] 执行，最后短借用写回
//!    （[`ImeState::store_detached`] 等）。只读的 `ImmGetContext` /
//!    `ImmGetCompositionStringW` 不派发消息，可以在借用内调用。
//! 2. **组合串按 flag 分项读取**：`WM_IME_COMPOSITION` 的 lParam 是 bitmask，
//!    `GCS_COMPSTR` / `GCS_CURSORPOS` / `GCS_COMPATTR` / `GCS_COMPCLAUSE` / `GCS_RESULTSTR`
//!    各是一次独立的查询；一次消息只 `ImmGetContext` 一次（旧实现每次查询各取一次）。
//! 3. **提交去重按 UTF-16 码元 + 内容校验**（不是码点计数）：IME 提交后系统补发的 `WM_CHAR`
//!    是**每码元一条**，提交串里的文本按码元排队，逐条比对 —— 内容不匹配立刻清空队列
//!    （异常 IME 自愈），失焦 / 重新组合 / 禁用 IME 也清空。
//!
//! 应用还可以**主动关闭**本窗口的 IME（自绘搜索框的英文直输模式）：
//! [`ImeState::plan_enabled`] 用 `ImmAssociateContext(hwnd, NULL)` 把输入上下文从窗口上摘下来
//! 并保存旧 `HIMC`，重新启用时还回去；禁用期间 `WndProc` 吞掉全部 `WM_IME_*`
//! （见 [`super::event_loop::wndproc`]）。

use std::collections::VecDeque;

use smol_str::SmolStr;
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::{
        Input::Ime::{
            ATTR_INPUT, CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, GCS_COMPATTR,
            GCS_COMPCLAUSE, GCS_COMPSTR, GCS_CURSORPOS, GCS_RESULTSTR, HIMC, IACE_DEFAULT,
            ISC_SHOWUICOMPOSITIONWINDOW, ISC_SHOWUIGUIDELINE, ImmAssociateContext,
            ImmAssociateContextEx, ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext,
            ImmSetCandidateWindow, ImmSetCompositionWindow,
        },
        WindowsAndMessaging::{WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION},
    },
};

use crate::{
    event::ImeEvent,
    math::PhysicalRect,
    platform::ime::{ClauseRanges, preedit_clauses},
};

/// `WM_IME_*` 的产物：应用事件 + 需要**在借用释放后**执行的 IMM32 命令。
#[derive(Debug, Default)]
pub(crate) struct ImeOutcome {
    /// 交给应用的 IME 事件。
    pub event: Option<ImeEvent>,
    /// 会同步重入 `WndProc` 的 IMM32 操作（由调用方在本模块之外执行）。
    pub command: Option<ImeCommand>,
}

/// 会**同步重入** `WndProc` 的 IMM32 操作（纯数据，执行见 [`execute`]）。
///
/// 单独成类型是 soundness 要求，不是抽象癖：`ImmAssociateContext*` / `ImmSet*Window` 会同步
/// 派发 `WM_IME_SETCONTEXT` / `WM_IME_NOTIFY`，重入的 `WndProc` 会再构造 `&mut WndProcState`；
/// 只要执行这些调用时不持有 `&mut`，别名 UB 就不成立（R07-7）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImeCommand {
    /// `ImmAssociateContext(hwnd, context)`：`None` = 摘下（NULL），`Some` = 还回保存的上下文。
    Associate(Option<HIMC>),
    /// `ImmAssociateContextEx(hwnd, NULL, IACE_DEFAULT)`：让系统关联**默认**上下文。
    AssociateDefault,
    /// `ImmSetCompositionWindow` + `ImmSetCandidateWindow`（客户区物理像素）。
    SetCaret(PhysicalRect),
}

/// 一条 [`ImeCommand`] 的执行结果（写回状态的唯一输入）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImeApplied {
    /// 已应用，无需写回。
    Done,
    /// 摘下上下文成功；`Some` = 摘下来的旧上下文（由调用方保存，窗口销毁前还回）。
    Detached(Option<HIMC>),
    /// `ImmAssociateContextEx(IACE_DEFAULT)` 失败：调用方必须把「已应用状态」回滚成禁用，
    /// 否则 `enabled` 已记账却永不重试 ⇒ IME 静默失效。
    RestoreFailed,
}

/// IME 组合状态（每个窗口一份，由 `WndProcState` 持有）。
#[derive(Debug)]
pub(crate) struct ImeState {
    /// 应用是否要求本窗口启用 IME（默认启用）。
    ///
    /// `false` 时窗口的输入上下文已从 IMM32 上摘下：`WndProc` 吞掉全部 `WM_IME_*`，
    /// 字母/数字键以普通 `WM_KEYDOWN` 到达应用（不再被 IME 变成 `VK_PROCESSKEY`）。
    enabled: bool,
    /// 禁用时 `ImmAssociateContext(hwnd, None)` 返回的旧上下文；重新启用 / 窗口销毁时还回去。
    ///
    /// IME 上下文由 IMM32 与窗口的关联管理：摘下来之后必须还回去，否则窗口销毁时系统
    /// 看不到它，上下文会泄漏。禁用时窗口本来就没有上下文（返回 NULL）⇒ `None`。
    saved_context: Option<HIMC>,
    /// 应用声明的 caret 区域（客户区物理像素）；`None` = 无 hint。
    cursor_area: Option<PhysicalRect>,
    /// 上一次提交文本的 **UTF-16 码元**队列：随后到达的同内容 `WM_CHAR` 要跳过。
    ///
    /// 用队列而不是计数（R07-5）：计数不知道吞的是什么，异常 IME / TSF 通路少发或重发一条
    /// `WM_CHAR` 就会把余数留给**用户之后真实键入**的字符（「输入 10 手，界面收到 1 手」）。
    /// 队列逐码元比对，不匹配即清空自愈。
    pending_chars: VecDeque<u16>,
}

impl Default for ImeState {
    fn default() -> Self {
        Self {
            enabled: true,
            saved_context: None,
            cursor_area: None,
            pending_chars: VecDeque::new(),
        }
    }
}

impl ImeState {
    /// 处理一条 `WM_IME_*`；返回事件 + 待执行的 IMM32 命令（可能都为空）。
    pub(crate) fn handle(&mut self, hwnd: HWND, msg: u32, lparam: isize) -> ImeOutcome {
        match msg {
            WM_IME_STARTCOMPOSITION => {
                // 组合开始时重放 caret 位置（上下文可能在禁用/启用之间换过，表单不保证还在）。
                // 这里的 `ImmSet*Window` 会重入 `WndProc`，所以只产命令、不在这里调用。
                ImeOutcome {
                    event: Some(ImeEvent::Start),
                    command: self.cursor_area.map(ImeCommand::SetCaret),
                }
            }
            WM_IME_COMPOSITION => {
                let flags = lparam as u32;
                if flags & GCS_RESULTSTR.0 != 0 {
                    // `ImmGetContext`/`ImmGetCompositionStringW` 只读组合状态，不派发窗口消息；
                    // `himc` 在 `composition_string` 内成对释放。
                    return match composition_string(hwnd, GCS_RESULTSTR) {
                        Some(text) => {
                            // 提交去重：按 **UTF-16 码元** 排队（emoji = 2 个码元，中文 = 1 个）。
                            self.queue_commit(&text);
                            ImeOutcome {
                                event: Some(ImeEvent::Commit {
                                    text,
                                    // WIN32 `GCS_RESULTSTR` 就是本次提交的完整串：IMM32 没有
                                    // AppKit `replacementRange` 那种「只替换组合串的一段」的
                                    // 区间信息，`None` = 替换整个当前组合串。
                                    replace: None,
                                }),
                                command: None,
                            }
                        }
                        None => ImeOutcome::default(),
                    };
                }
                if flags & GCS_COMPSTR.0 != 0 {
                    let Some(read) = read_composition(hwnd, flags) else {
                        return ImeOutcome::default();
                    };
                    return ImeOutcome {
                        event: Some(ImeEvent::Preedit {
                            text: read.text,
                            caret: read.caret,
                            clauses: read.clauses,
                            // `GCS_COMPSTR` 是**完整**组合串（不是增量），所以整体替换。
                            replace: None,
                        }),
                        command: None,
                    };
                }
                ImeOutcome::default()
            }
            WM_IME_ENDCOMPOSITION => ImeOutcome {
                event: Some(ImeEvent::End),
                command: None,
            },
            _ => ImeOutcome::default(),
        }
    }

    /// `WM_CHAR` 是否应被 IME 提交吞掉（提交后 Windows 会再发同样的字符）。
    ///
    /// 只吞**与队首一致**的码元；不一致立刻清空队列并放行（异常 IME 自愈，绝不吞掉真实键入）。
    pub(crate) fn consume_char(&mut self, unit: u16) -> bool {
        match self.pending_chars.front() {
            Some(&expected) if expected == unit => {
                self.pending_chars.pop_front();
                true
            }
            Some(_) => {
                // 提交文本与真实到达的字符不一致：队列作废（剩下的码元不会再来）。
                self.pending_chars.clear();
                false
            }
            None => false,
        }
    }

    /// 把一次提交的文本排进待抑制队列（随后的 `WM_CHAR` 逐码元比对后跳过）。
    ///
    /// 单位是 **UTF-16 码元**：系统在 IME 提交之后补发的 `WM_CHAR` 每码元一条，
    /// 「🌍a」= 3 条。用码点数（旧实现的 `chars().count()`）会让 emoji 提交后重复插入
    /// 或漏抑制（R07-5 / R02-13）。
    pub(crate) fn queue_commit(&mut self, text: &str) {
        self.pending_chars = text.encode_utf16().collect();
    }

    /// 清空待抑制队列（失焦 / 重新组合 / 禁用 IME）。
    pub(crate) fn clear_pending(&mut self) {
        self.pending_chars.clear();
    }

    /// 本窗口的 IME 当前是否启用（`WndProc` 据此吞掉 `WM_IME_*`）。
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// 计划一次启用/禁用（应用目标状态与已应用状态不同时才调用）。
    ///
    /// 记账在**返回命令之前**完成：`ImmAssociateContext*` 会同步派发 `WM_IME_SETCONTEXT`，
    /// 嵌套进入的 `WndProc` 必须已经看到新状态（禁用期间那条消息要被吞掉，否则默认过程
    /// 可能把组合窗重新显示出来）。
    pub(crate) fn plan_enabled(&mut self, enabled: bool) -> Option<ImeCommand> {
        if enabled == self.enabled {
            return None;
        }
        self.enabled = enabled;
        if enabled {
            // 恢复：保存过上下文就还回去，否则让系统给窗口关联默认上下文。
            Some(match self.saved_context.take() {
                Some(context) => ImeCommand::Associate(Some(context)),
                None => ImeCommand::AssociateDefault,
            })
        } else {
            // 禁用：摘下上下文，并清掉未消费的抑制队列（否则重新启用后第一次真实输入会被
            // 上一次提交的余数吞掉）。
            self.pending_chars.clear();
            Some(ImeCommand::Associate(None))
        }
    }

    /// 禁用态下收到 `WM_IME_SETCONTEXT` / `WM_INPUTLANGCHANGE`：把系统**重新关联**回窗口的
    /// 输入上下文再摘一次（幂等；win-1 P3）。
    ///
    /// IMM32 在焦点 / 输入法切换后可能重新给窗口关联上下文（MSDN：系统默认给每个窗口关联
    /// 默认输入上下文；`WM_IME_SETCONTEXT` 正是「上下文关联状态变了」的通知）。只摘一次的话，
    /// 禁用期间窗口又有了上下文 ⇒ 字母键重新被 IME 吃成组合输入。
    pub(crate) fn plan_reassert_disabled(&mut self) -> Option<ImeCommand> {
        if self.enabled {
            return None;
        }
        Some(ImeCommand::Associate(None))
    }

    /// 窗口销毁**之前**把摘下来的上下文还回去，让系统随窗口释放它（避免 `HIMC` 泄漏）。
    ///
    /// 必须在 `DestroyWindow` 之前、HWND 仍有效时执行。没有保存过上下文则无事可做：
    /// 禁用时窗口本来就没有上下文，不存在本层拥有的句柄。
    pub(crate) fn plan_release_before_destroy(&mut self) -> Option<ImeCommand> {
        self.enabled = true;
        self.pending_chars.clear();
        self.saved_context
            .take()
            .map(|context| ImeCommand::Associate(Some(context)))
    }

    /// 保存并计划立刻应用 caret 区域（`None` = 清除 hint）。
    pub(crate) fn plan_cursor_area(&mut self, area: Option<PhysicalRect>) -> Option<ImeCommand> {
        self.cursor_area = area;
        area.map(ImeCommand::SetCaret)
    }

    /// 写回「摘下来的旧上下文」：**覆盖**保存值（禁用路径的唯一写点）。
    pub(crate) fn store_detached(&mut self, detached: Option<HIMC>) {
        self.saved_context = detached;
    }

    /// 写回「禁用态下重新摘下来的上下文」：**只在还没有保存值时**记录。
    ///
    /// 已经有保存值时不覆盖：系统这次重新关联出来的上下文只被摘下、不再关联 —— IMM32 的
    /// 默认输入上下文是线程级的，本层不 `ImmDestroyContext` 它（擅自销毁会让后续 IME 输入
    /// 失效），由输入法/线程生命周期回收。
    pub(crate) fn store_reasserted(&mut self, detached: Option<HIMC>) {
        if self.saved_context.is_none() {
            self.saved_context = detached;
        }
    }

    /// 写回 [`ImeApplied`]（唯一的状态回写入口，见 [`super::event_loop::WndProcState::note_ime_applied`]）。
    pub(crate) fn note_applied(&mut self, applied: ImeApplied) {
        match applied {
            ImeApplied::Done => {}
            ImeApplied::Detached(context) => self.store_detached(context),
            ImeApplied::RestoreFailed => {
                // 恢复失败：回滚成「禁用」，下一轮泵会重试；否则 `enabled` 已记账、永远不会
                // 再试 ⇒ IME 静默失效（复审 W1-4）。
                self.enabled = false;
            }
        }
    }

    /// 写回「禁用态重新摘除」的结果：**不覆盖**已保存的待恢复上下文。
    pub(crate) fn note_reasserted(&mut self, applied: ImeApplied) {
        match applied {
            ImeApplied::Detached(context) => self.store_reasserted(context),
            other => self.note_applied(other),
        }
    }
}

/// 执行一条 [`ImeCommand`]。
///
/// **调用方不得持有 `&mut WndProcState`**：这三个 API 都会同步派发 `WM_IME_*`，嵌套进入的
/// `WndProc` 会再构造 `&mut WndProcState`（R07-7 的别名 UB 就是从这里来的）。
pub(crate) fn execute(hwnd: HWND, command: ImeCommand) -> ImeApplied {
    // SAFETY: `hwnd` 由调用方保证存活（窗口销毁前 / 消息处理期间）；这些 API 只改
    // 「窗口 ↔ 输入上下文」的关联或组合/候选窗表单，不持有 Rust 引用、不保留指针。
    unsafe {
        match command {
            ImeCommand::Associate(context) => {
                let previous = ImmAssociateContext(hwnd, context.unwrap_or_default());
                if context.is_none() {
                    ImeApplied::Detached((!previous.is_invalid()).then_some(previous))
                } else {
                    // 还回保存的上下文：返回值是「被替换出来的旧上下文」（窗口刚被摘下，
                    // 正常为 NULL；若禁用期间系统又关联了新上下文，它随窗口释放，本层不释放）。
                    ImeApplied::Done
                }
            }
            ImeCommand::AssociateDefault => {
                if ImmAssociateContextEx(hwnd, HIMC::default(), IACE_DEFAULT).as_bool() {
                    ImeApplied::Done
                } else {
                    ImeApplied::RestoreFailed
                }
            }
            ImeCommand::SetCaret(area) => {
                set_caret_forms(hwnd, area);
                ImeApplied::Done
            }
        }
    }
}

/// `WM_IME_SETCONTEXT` 的 lParam 掩码：清掉 IME **自绘**组合窗与 guideline，保留其它位。
///
/// 引擎自己画 inline preedit（[`ImeEvent::Preedit`]）；不清 `ISC_SHOWUICOMPOSITIONWINDOW`
/// 的话 IME 会在 caret 处**同时**画自己的组合窗 —— 用户看到「输入的拼音出现两遍」
/// （Chrome `RenderWidgetHostViewWin` 与 Qt `QWindowsInputContext` 都清这一位；MS 官方
/// IME 教程直接示范 `lParam = 0` 再 `DefWindowProc`）。
///
/// **候选窗保留**（`ISC_SHOWUICANDIDATEWINDOW`）：引擎只画组合串，候选列表仍由 IME 画。
/// `wParam == FALSE`（上下文失活）时原样返回：此时只是状态通知，没有可见窗口要改。
pub(crate) fn mask_setcontext(wparam: usize, lparam: isize) -> isize {
    if wparam == 0 {
        return lparam;
    }
    // lParam 是 32 位 IME 窗口标志（64 位下由系统符号扩展/零扩展传入），按 u32 处理。
    let flags = lparam as u32;
    let masked = flags & !(ISC_SHOWUICOMPOSITIONWINDOW | ISC_SHOWUIGUIDELINE);
    masked as isize
}

/// 一次 `WM_IME_COMPOSITION` 读到的组合数据。
struct CompositionRead {
    text: SmolStr,
    /// 组合串内插入点（UTF-16 码元，已夹到 `0..=len`）；`None` = 本次消息没带该 flag。
    caret: Option<u32>,
    clauses: ClauseRanges,
}

/// 按 flag 分项读取组合串（**一次** `ImmGetContext` 覆盖全部查询）。
fn read_composition(hwnd: HWND, flags: u32) -> Option<CompositionRead> {
    // SAFETY: `himc` 在函数内成对释放；所有缓冲区都由本函数独占，写入之后才读。
    unsafe {
        let himc: HIMC = ImmGetContext(hwnd);
        if himc.is_invalid() {
            return None;
        }
        let text = composition_string_with(himc, GCS_COMPSTR);
        let caret = (flags & GCS_CURSORPOS.0 != 0)
            .then(|| cursor_position(himc))
            .flatten();
        let attrs = (flags & GCS_COMPATTR.0 != 0).then(|| composition_bytes(himc, GCS_COMPATTR));
        let clause_offsets =
            (flags & GCS_COMPCLAUSE.0 != 0).then(|| composition_dwords(himc, GCS_COMPCLAUSE));
        let _ = ImmReleaseContext(hwnd, himc);
        let text = text?;
        // 长度必须是 **UTF-16 码元**数：属性字节与子句边界都按码元计数。
        let len = text.encode_utf16().count();
        let clauses = preedit_clauses(
            attrs.as_deref(),
            clause_offsets.as_deref(),
            len,
            // `ATTR_INPUT`（0）是「仍在输入、不需要高亮」的属性；其余（已转换/转换中/
            // 已固定/输入错误）都交给文本层高亮。
            ATTR_INPUT as u8,
        );
        Some(CompositionRead {
            text,
            caret: caret.map(|position| clamp_caret(position, len)),
            clauses,
        })
    }
}

/// 组合串内插入点夹到 `0..=len`。
///
/// `GCS_CURSORPOS` 在第三方 IME / 组合串刚清空时会给出越界值（甚至超过 `i16::MAX` 的伪造值），
/// 直接交给文本层会切在非码点边界或越界 panic；夹住即可（视觉上退化为末尾）。
fn clamp_caret(position: u32, len: usize) -> u32 {
    (position as usize).min(len) as u32
}

/// `ImmGetCompositionStringW(GCS_CURSORPOS)`：**返回值就是插入点**，不是字节数。
///
/// 这是 IMM32 唯一一个返回「值」而不是「写入字节数」的 flag；把它当字节数去分配缓冲区是
/// 常见错误（R07-4）。负返回值（`IMM_ERROR_NODATA` / `IMM_ERROR_GENERAL`）⇒ `None`。
fn cursor_position(himc: HIMC) -> Option<u32> {
    // SAFETY: `himc` 有效（调用方由 `ImmGetContext` 取得）；不写缓冲区。
    let position = unsafe { ImmGetCompositionStringW(himc, GCS_CURSORPOS, None, 0) };
    (position >= 0).then_some(position as u32)
}

/// 取组合串（UTF-16 → `SmolStr`）；上下文无效或长度 ≤ 0 时返回 `None`。
fn composition_string(
    hwnd: HWND,
    flag: windows::Win32::UI::Input::Ime::IME_COMPOSITION_STRING,
) -> Option<SmolStr> {
    // SAFETY: `himc` 成对释放；缓冲区由本函数独占，`written` 之后才读。
    unsafe {
        let himc: HIMC = ImmGetContext(hwnd);
        if himc.is_invalid() {
            return None;
        }
        let text = composition_string_with(himc, flag);
        let _ = ImmReleaseContext(hwnd, himc);
        text
    }
}

/// [`composition_string`] 的 `himc` 版本（组合串读取共用一次上下文）。
fn composition_string_with(
    himc: HIMC,
    flag: windows::Win32::UI::Input::Ime::IME_COMPOSITION_STRING,
) -> Option<SmolStr> {
    // SAFETY: `himc` 有效；先问长度，再按长度分配缓冲区写入。
    unsafe {
        let bytes = ImmGetCompositionStringW(himc, flag, None, 0);
        if bytes <= 0 {
            return None;
        }
        let mut units = vec![0u16; (bytes as usize).div_ceil(2)];
        let written = ImmGetCompositionStringW(
            himc,
            flag,
            Some(units.as_mut_ptr().cast()),
            (units.len() * 2) as u32,
        );
        if written <= 0 {
            return None;
        }
        let len = written as usize / 2;
        Some(SmolStr::new(String::from_utf16_lossy(&units[..len])))
    }
}

/// `ImmGetCompositionStringW` 的按字节输出项（`GCS_COMPATTR`：每码元 1 字节）。
fn composition_bytes(
    himc: HIMC,
    flag: windows::Win32::UI::Input::Ime::IME_COMPOSITION_STRING,
) -> Vec<u8> {
    // SAFETY: 两段式：先问字节数，再按同一长度写入本函数独占的缓冲区。
    unsafe {
        let bytes = ImmGetCompositionStringW(himc, flag, None, 0);
        if bytes <= 0 {
            return Vec::new();
        }
        let mut buffer = vec![0u8; bytes as usize];
        let written =
            ImmGetCompositionStringW(himc, flag, Some(buffer.as_mut_ptr().cast()), bytes as u32);
        if written <= 0 {
            return Vec::new();
        }
        buffer.truncate(written as usize);
        buffer
    }
}

/// `ImmGetCompositionStringW` 的按 `u32` 输出项（`GCS_COMPCLAUSE`：子句边界偏移）。
fn composition_dwords(
    himc: HIMC,
    flag: windows::Win32::UI::Input::Ime::IME_COMPOSITION_STRING,
) -> Vec<u32> {
    // SAFETY: 同 `composition_bytes`；长度不足一个 `u32` 的尾巴被丢弃。
    unsafe {
        let bytes = ImmGetCompositionStringW(himc, flag, None, 0);
        if bytes < 4 {
            return Vec::new();
        }
        let mut buffer = vec![0u32; bytes as usize / 4];
        let written = ImmGetCompositionStringW(
            himc,
            flag,
            Some(buffer.as_mut_ptr().cast()),
            (buffer.len() * 4) as u32,
        );
        if written < 4 {
            return Vec::new();
        }
        buffer.truncate(written as usize / 4);
        buffer
    }
}

/// 把 caret 区域写成组合窗 + 候选窗表单（客户区物理像素）。
fn set_caret_forms(hwnd: HWND, area: PhysicalRect) {
    let (composition, candidate) = caret_forms(area);
    // SAFETY: `himc` 成对释放；两个 form 都是栈上值，API 不保留指针。
    unsafe {
        let himc: HIMC = ImmGetContext(hwnd);
        if himc.is_invalid() {
            return;
        }
        let _ = ImmSetCompositionWindow(himc, &composition);
        let _ = ImmSetCandidateWindow(himc, &candidate);
        let _ = ImmReleaseContext(hwnd, himc);
    }
}

/// caret 区域 → `(COMPOSITIONFORM, CANDIDATEFORM)`。
///
/// - 组合窗落点：caret 区域底边（`CFS_POINT`）；
/// - 候选窗：左上角 + `CFS_EXCLUDE`，让 IME 避开文字区域。
///
/// 坐标相对**客户区**左上角（MSDN：`COMPOSITIONFORM` 与 `CANDIDATEFORM` 都是客户区坐标），
/// 因此调用方传客户区物理像素即可。
fn caret_forms(area: PhysicalRect) -> (COMPOSITIONFORM, CANDIDATEFORM) {
    let width = area.width().max(1);
    let height = area.height().max(1);
    let rc_area = RECT {
        left: area.x0,
        top: area.y0,
        right: area.x0 + width,
        bottom: area.y0 + height,
    };
    (
        COMPOSITIONFORM {
            dwStyle: CFS_POINT,
            ptCurrentPos: POINT {
                x: area.x0,
                y: area.y0 + height,
            },
            rcArea: rc_area,
        },
        CANDIDATEFORM {
            dwIndex: 0,
            dwStyle: CFS_EXCLUDE,
            ptCurrentPos: POINT {
                x: area.x0,
                y: area.y0,
            },
            rcArea: rc_area,
        },
    )
}

#[cfg(test)]
mod tests {
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::Ime::{
                CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, HIMC,
                ISC_SHOWUICANDIDATEWINDOW, ISC_SHOWUICOMPOSITIONWINDOW, ISC_SHOWUIGUIDELINE,
                ImmGetCandidateWindow, ImmGetCompositionWindow, ImmGetContext, ImmReleaseContext,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WINDOW_EX_STYLE,
                WNDCLASSW, WS_OVERLAPPEDWINDOW,
            },
        },
    };

    use super::{
        ImeApplied, ImeCommand, ImeState, caret_forms, clamp_caret, execute, mask_setcontext,
    };
    use crate::math::PhysicalRect;

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: 测试窗口只用于 IME 表单读写，默认过程不依赖额外状态。
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 建一个隐藏窗口（不 `ShowWindow`）。
    fn hidden_window(tag: &str) -> (HWND, Vec<u16>) {
        let class_name = wide(tag);
        // SAFETY: 进程级模块句柄；类名是 NUL 结尾宽字符串，`RegisterClassW` 同步拷贝。
        let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&class) };
        assert_ne!(atom, 0, "RegisterClassW");
        // SAFETY: 类已注册；`WS_OVERLAPPEDWINDOW` 不含 `WS_VISIBLE` ⇒ 窗口保持隐藏。
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::PCWSTR(class_name.as_ptr()),
                windows::core::PCWSTR(class_name.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                320,
                240,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
        .expect("CreateWindowExW");
        (hwnd, class_name)
    }

    /// 两阶段驱动一次命令（与生产 `WndProcState::note_ime_applied` 同序）。
    fn drive(state: &mut ImeState, hwnd: HWND, command: Option<ImeCommand>) {
        if let Some(command) = command {
            let applied = execute(hwnd, command);
            state.note_applied(applied);
        }
    }

    /// 两阶段驱动一次「禁用态重新摘除」（与生产 `WndProcState::note_ime_reasserted` 同序）。
    fn drive_reassert(state: &mut ImeState, hwnd: HWND, command: Option<ImeCommand>) {
        if let Some(command) = command {
            let applied = execute(hwnd, command);
            state.note_reasserted(applied);
        }
    }

    #[test]
    fn caret_forms_map_to_composition_and_candidate_windows() {
        let area = PhysicalRect::new(100, 200, 101, 220);

        let (composition, candidate) = caret_forms(area);

        assert_eq!(composition.dwStyle, CFS_POINT);
        assert_eq!(composition.ptCurrentPos.x, 100);
        assert_eq!(composition.ptCurrentPos.y, 220);
        assert_eq!(candidate.dwStyle, CFS_EXCLUDE);
        assert_eq!(candidate.dwIndex, 0);
        assert_eq!(candidate.ptCurrentPos.x, 100);
        assert_eq!(candidate.ptCurrentPos.y, 200);
        assert_eq!(candidate.rcArea.left, 100);
        assert_eq!(candidate.rcArea.top, 200);
        assert_eq!(candidate.rcArea.right, 101);
        assert_eq!(candidate.rcArea.bottom, 220);
    }

    #[test]
    fn caret_forms_follow_physical_scale() {
        let logical = crate::math::Rect::new(100.0, 200.0, 1.0, 18.0);
        let physical = logical.to_physical(crate::math::Scale::new(1.25).expect("scale"));

        let (composition, candidate) = caret_forms(physical);

        assert_eq!(composition.ptCurrentPos.x, 125);
        assert_eq!(composition.ptCurrentPos.y, 273);
        assert_eq!(candidate.rcArea.left, 125);
        assert_eq!(candidate.rcArea.top, 250);
        assert_eq!(candidate.rcArea.right, 126);
        assert_eq!(candidate.rcArea.bottom, 273);
    }

    /// R07-3：引擎自己画 inline preedit ⇒ 必须清掉 IME 自绘组合窗与 guideline，候选窗保留。
    #[test]
    fn setcontext_masking_clears_the_ime_composition_window_and_guideline() {
        let lparam = (ISC_SHOWUICOMPOSITIONWINDOW | ISC_SHOWUIGUIDELINE | ISC_SHOWUICANDIDATEWINDOW)
            as isize;

        let masked = mask_setcontext(1, lparam);

        assert_eq!(
            masked & ISC_SHOWUICOMPOSITIONWINDOW as isize,
            0,
            "组合窗开关必须清零（否则 IME 会与 inline preedit 双显）"
        );
        assert_eq!(
            masked & ISC_SHOWUIGUIDELINE as isize,
            0,
            "guideline 与组合串同一套自绘，必须一起清"
        );
        assert_ne!(
            masked & ISC_SHOWUICANDIDATEWINDOW as isize,
            0,
            "候选窗仍由 IME 画，不能清"
        );
    }

    /// 上下文失活（wParam == FALSE）时系统只是通知状态，不得改 lParam。
    #[test]
    fn setcontext_masking_leaves_the_inactive_notification_alone() {
        let lparam = ISC_SHOWUICOMPOSITIONWINDOW as isize;
        assert_eq!(mask_setcontext(0, lparam), lparam);
        // wParam == TRUE 且没有任何标志位时结果仍是 0（不引入新位）。
        assert_eq!(mask_setcontext(1, 0), 0);
    }

    /// R07-5 / R02-13：提交去重按 **UTF-16 码元** 排队，emoji（2 码元）之后不能多吞。
    #[test]
    fn committed_astral_text_queues_one_unit_per_utf16_code_unit() {
        // 生产路径：`pending_chars = text.encode_utf16().collect()`；这里直接模拟同样的
        // 队列（避免构造真实 HIMC）：「🌍a」= 高代理 + 低代理 + 'a'。
        let mut state = ImeState {
            pending_chars: "🌍a".encode_utf16().collect(),
            ..ImeState::default()
        };
        assert_eq!(state.pending_chars.len(), 3, "🌍a = 3 个码元");

        assert!(state.consume_char(0xD83C));
        assert!(state.consume_char(0xDF0D));
        assert!(state.consume_char(u16::from(b'a')));
        assert!(
            !state.consume_char(u16::from(b'b')),
            "队列清空后必须放行真实键入"
        );
    }

    /// 内容不匹配立即自愈：不能把余数留给用户之后真实键入的字符。
    #[test]
    fn a_mismatched_char_clears_the_queue_instead_of_swallowing_later_input() {
        let mut state = ImeState {
            pending_chars: "你好".encode_utf16().collect(),
            ..ImeState::default()
        };

        // IME 通路没送来提交后的 `WM_CHAR`，用户直接键入 'x'：不吞、并清空队列。
        assert!(!state.consume_char(u16::from(b'x')));
        assert!(!state.consume_char(u16::from(b'y')), "残留队列必须已清空");
    }

    /// 重新组合 / 失焦 / 禁用都会清空队列（`plan_enabled(false)` 与 `clear_pending`）。
    #[test]
    fn disabling_the_ime_drops_the_pending_characters() {
        let mut state = ImeState {
            pending_chars: "ni".encode_utf16().collect(),
            ..ImeState::default()
        };

        let command = state.plan_enabled(false);

        assert!(
            matches!(command, Some(ImeCommand::Associate(None))),
            "禁用必须产出摘除上下文的命令"
        );
        assert!(state.pending_chars.is_empty(), "禁用必须清空待抑制队列");
        assert!(!state.enabled());
    }

    /// `GCS_CURSORPOS` 越界（第三方 IME / 组合串刚清空）时必须夹到 `0..=len`。
    #[test]
    fn the_composition_caret_is_clamped_to_the_composition_length() {
        assert_eq!(clamp_caret(0, 3), 0);
        assert_eq!(clamp_caret(2, 3), 2);
        assert_eq!(clamp_caret(3, 3), 3);
        assert_eq!(clamp_caret(99, 3), 3, "越界值退化为末尾");
        assert_eq!(clamp_caret(u32::MAX, 0), 0);
    }

    /// 启用态下 `plan_reassert_disabled` 不得产出命令（只在禁用态摘上下文）。
    #[test]
    fn reassert_is_a_no_op_while_the_ime_is_enabled() {
        let mut state = ImeState::default();
        assert!(state.plan_reassert_disabled().is_none());
    }

    /// `ImeApplied::RestoreFailed` 必须把已应用状态回滚成禁用（否则永不重试）。
    #[test]
    fn a_failed_restore_rolls_the_applied_state_back_to_disabled() {
        let mut state = ImeState::default();
        state.plan_enabled(false);
        state.plan_enabled(true);
        assert!(state.enabled(), "前置：记账为启用");

        state.note_applied(ImeApplied::RestoreFailed);

        assert!(!state.enabled(), "恢复失败必须回滚，下一轮泵会重试");
    }

    #[test]
    fn unknown_messages_do_not_produce_ime_events() {
        let mut state = ImeState::default();
        // SAFETY: 该消息分支不读 `hwnd`；传空句柄是安全的（只走 `_` 分支）。
        let outcome = state.handle(
            HWND::default(),
            windows::Win32::UI::WindowsAndMessaging::WM_PAINT,
            0,
        );

        assert!(outcome.event.is_none());
        assert!(outcome.command.is_none());
    }

    #[test]
    fn hidden_window_can_read_back_caret_forms() {
        let (hwnd, _class) = hidden_window("KytrixP16ImeForms");
        let mut state = ImeState::default();
        let command = state.plan_cursor_area(Some(PhysicalRect::new(100, 200, 101, 220)));
        drive(&mut state, hwnd, command);
        // SAFETY: 窗口由本用例创建；上下文成对释放，窗口最后销毁。
        unsafe {
            let himc = ImmGetContext(hwnd);
            if himc.is_invalid() {
                eprintln!("skip: hidden window has no IME context");
                let _ = DestroyWindow(hwnd);
                return;
            }
            let mut composition = COMPOSITIONFORM::default();
            let mut candidate = CANDIDATEFORM::default();
            if ImmGetCompositionWindow(himc, &mut composition).as_bool() {
                // CFS_POINT 允许 IME 调整 ptCurrentPos；这里只核对 rcArea。
                assert_eq!(composition.rcArea.left, 100);
                assert_eq!(composition.rcArea.top, 200);
            } else {
                eprintln!("skip: ImmGetCompositionWindow returned false");
            }
            if ImmGetCandidateWindow(himc, 0, &mut candidate).as_bool() {
                assert_eq!(candidate.rcArea.left, 100);
                assert_eq!(candidate.rcArea.top, 200);
            } else {
                eprintln!("skip: ImmGetCandidateWindow returned false");
            }
            let _ = ImmReleaseContext(hwnd, himc);
            let _ = DestroyWindow(hwnd);
        }
    }

    /// win-1 P3（任务 2）：窗口销毁前的恢复通路必须真的把摘下的上下文还回去。
    ///
    /// 覆盖缺口：删掉 NativeWindow::destroy 里的 plan_ime_release 执行时，既有用例
    /// 全绿也抓不到。本用例直接驱动保存/恢复状态机（两阶段：命令 + 写回）；
    /// `platform/windows/window.rs` 的接线用例覆盖 `NativeWindow::sync_ime_enabled` 与
    /// `NativeWindow::destroy` 这两个调用点（泵每轮调用它们的**节奏**需要真实泵 + 渲染器）。
    #[test]
    fn release_before_destroy_restores_the_saved_ime_context() {
        let (hwnd, _class) = hidden_window("KytrixP16ImeRelease");
        let mut state = ImeState::default();
        // SAFETY: 窗口由本用例创建且存活；`ImmGetContext` 的句柄成对释放，窗口最后销毁。
        unsafe {
            let baseline = ImmGetContext(hwnd);
            if baseline.is_invalid() {
                eprintln!("skip: hidden window has no IME context");
                let _ = DestroyWindow(hwnd);
                return;
            }
            let _ = ImmReleaseContext(hwnd, baseline);

            let command = state.plan_enabled(false);
            drive(&mut state, hwnd, command);
            assert!(
                ImmGetContext(hwnd).is_invalid(),
                "禁用后窗口不得再关联输入上下文"
            );

            let command = state.plan_release_before_destroy();
            drive(&mut state, hwnd, command);
            let restored = ImmGetContext(hwnd);
            assert!(
                !restored.is_invalid(),
                "销毁前必须把保存的上下文还回窗口，否则 HIMC 泄漏"
            );
            assert_eq!(restored, baseline, "恢复的必须是被摘下的那个上下文");
            // 恢复后 `enabled` 必须回到 true：窗口销毁阶段不再有禁用态。
            assert!(state.enabled());
            let _ = ImmReleaseContext(hwnd, restored);

            // 启用态下「幂等再摘除」不得动上下文（只应在禁用态触发）。
            let command = state.plan_reassert_disabled();
            drive_reassert(&mut state, hwnd, command);
            let still_attached = ImmGetContext(hwnd);
            assert!(
                !still_attached.is_invalid(),
                "启用态收到 IME 通知不得摘除上下文"
            );
            let _ = ImmReleaseContext(hwnd, still_attached);
            let _ = DestroyWindow(hwnd);
        }
    }

    /// 禁用态下系统重新关联上下文时，`plan_reassert_disabled` 必须再摘一次，且**不覆盖**
    /// 已保存的待恢复上下文。
    #[test]
    fn reasserting_a_disabled_ime_detaches_again_without_overwriting_the_saved_context() {
        let (hwnd, _class) = hidden_window("KytrixP16ImeReassert");
        let mut state = ImeState::default();
        // SAFETY: 窗口由本用例创建且存活。
        unsafe {
            let baseline = ImmGetContext(hwnd);
            if baseline.is_invalid() {
                eprintln!("skip: hidden window has no IME context");
                let _ = DestroyWindow(hwnd);
                return;
            }
            let _ = ImmReleaseContext(hwnd, baseline);

            let command = state.plan_enabled(false);
            drive(&mut state, hwnd, command);
            let saved = state.saved_context;
            assert!(saved.is_some(), "禁用时必须保存被摘下的上下文");

            // 系统把上下文重新挂回窗口（模拟焦点/输入法切换）。
            let _ = super::ImmAssociateContext(hwnd, saved.expect("saved context"));
            // 再摘一次：返回的应是刚才挂回去的那个，但保存值不得被新值覆盖。
            let command = state.plan_reassert_disabled();
            drive_reassert(&mut state, hwnd, command);
            assert!(ImmGetContext(hwnd).is_invalid(), "重新关联后必须再摘一次");
            assert_eq!(state.saved_context, saved, "不得覆盖待恢复的上下文");

            let _ = DestroyWindow(hwnd);
        }
    }

    /// `saved_context` 为 `None` 时（禁用时窗口本来无上下文），恢复走 `IACE_DEFAULT`。
    #[test]
    fn restoring_without_a_saved_context_asks_the_system_for_the_default_one() {
        let mut state = ImeState::default();
        state.plan_enabled(false);
        // 手动把保存值清掉，模拟「禁用时窗口本来就没有上下文」。
        state.saved_context = None;

        let command = state.plan_enabled(true);

        assert!(
            matches!(command, Some(ImeCommand::AssociateDefault)),
            "没有保存值时必须让系统关联默认上下文"
        );
        assert!(state.enabled());
    }

    /// 类型守卫：`HIMC` 是 `Copy` 的句柄，命令可以按值传递（两阶段执行的前提）。
    #[test]
    fn commands_are_plain_data_that_can_outlive_the_borrow() {
        fn takes(_: ImeCommand) {}
        takes(ImeCommand::Associate(Some(HIMC::default())));
        takes(ImeCommand::Associate(None));
        takes(ImeCommand::AssociateDefault);
        takes(ImeCommand::SetCaret(PhysicalRect::new(0, 0, 1, 1)));
    }
}

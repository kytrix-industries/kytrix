use std::collections::VecDeque;

use smallvec::{SmallVec, smallvec};
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

#[derive(Debug, Default)]
pub(crate) struct ImeOutcome {
    pub events: SmallVec<[ImeEvent; 2]>,
    pub command: Option<ImeCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImeCommand {
    Associate(Option<HIMC>),
    AssociateDefault,
    SetCaret(PhysicalRect),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImeApplied {
    Done,
    Detached(Option<HIMC>),
    RestoreFailed,
}

#[derive(Debug)]
pub(crate) struct ImeState {
    enabled: bool,
    saved_context: Option<HIMC>,
    cursor_area: Option<PhysicalRect>,
    pending_chars: VecDeque<u16>,
    pending_batch: Option<u64>,
    pending_ticks: u32,
    batch: u64,
    composing: bool,
}

const PENDING_BATCH_LIFETIME: u64 = 1;

const PENDING_COMMIT_WINDOW_MS: u32 = 250;

impl Default for ImeState {
    fn default() -> Self {
        Self {
            enabled: true,
            saved_context: None,
            cursor_area: None,
            pending_chars: VecDeque::new(),
            pending_batch: None,
            pending_ticks: 0,
            batch: 0,
            composing: false,
        }
    }
}

impl ImeState {
    pub(crate) fn handle(
        &mut self,
        hwnd: HWND,
        msg: u32,
        lparam: isize,
        ticks_ms: u32,
    ) -> ImeOutcome {
        match msg {
            WM_IME_STARTCOMPOSITION => {
                self.clear_pending();
                self.composing = true;
                ImeOutcome {
                    events: smallvec![ImeEvent::Start],
                    command: self.cursor_area.map(ImeCommand::SetCaret),
                }
            }
            WM_IME_COMPOSITION => {
                let flags = lparam as u32;
                let committed = if flags & GCS_RESULTSTR.0 != 0 {
                    composition_string(hwnd, GCS_RESULTSTR)
                } else {
                    None
                };
                if let Some(text) = committed.as_ref() {
                    self.queue_commit(text, ticks_ms);
                }
                let read = if flags & GCS_COMPSTR.0 != 0 {
                    self.composing = true;
                    read_composition(hwnd, flags)
                } else {
                    None
                };
                ImeOutcome {
                    events: composition_events(flags, committed, read),
                    command: None,
                }
            }
            WM_IME_ENDCOMPOSITION => {
                let composing = std::mem::take(&mut self.composing);
                ImeOutcome {
                    events: if composing {
                        smallvec![ImeEvent::End]
                    } else {
                        SmallVec::new()
                    },
                    command: None,
                }
            }
            _ => ImeOutcome::default(),
        }
    }

    pub(crate) fn is_composing(&self) -> bool {
        self.composing
    }

    pub(crate) fn finish_composition(&mut self) -> Option<ImeEvent> {
        self.clear_pending();
        std::mem::take(&mut self.composing).then_some(ImeEvent::End)
    }

    pub(crate) fn consume_char(&mut self, unit: u16, ticks_ms: u32) -> bool {
        if self.pending_chars.is_empty() {
            return false;
        }
        if !self.pending_is_live(ticks_ms) {
            self.clear_pending();
            return false;
        }
        match self.pending_chars.front() {
            Some(&expected) if expected == unit => {
                self.pending_chars.pop_front();
                if self.pending_chars.is_empty() {
                    self.pending_batch = None;
                }
                true
            }
            Some(_) => {
                self.clear_pending();
                false
            }
            None => false,
        }
    }

    fn pending_is_live(&self, ticks_ms: u32) -> bool {
        let Some(queued_batch) = self.pending_batch else {
            return false;
        };
        self.batch.saturating_sub(queued_batch) <= PENDING_BATCH_LIFETIME
            && ticks_ms.wrapping_sub(self.pending_ticks) <= PENDING_COMMIT_WINDOW_MS
    }

    pub(crate) fn queue_commit(&mut self, text: &str, ticks_ms: u32) {
        self.pending_chars.extend(text.encode_utf16());
        self.pending_batch = Some(self.batch);
        self.pending_ticks = ticks_ms;
    }

    pub(crate) fn end_message_batch(&mut self) {
        self.batch = self.batch.wrapping_add(1);
    }

    pub(crate) fn clear_pending(&mut self) {
        self.pending_chars.clear();
        self.pending_batch = None;
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn plan_enabled(&mut self, enabled: bool) -> Option<ImeCommand> {
        if enabled == self.enabled {
            return None;
        }
        self.enabled = enabled;
        if enabled {
            Some(match self.saved_context.take() {
                Some(context) => ImeCommand::Associate(Some(context)),
                None => ImeCommand::AssociateDefault,
            })
        } else {
            self.clear_pending();
            self.composing = false;
            Some(ImeCommand::Associate(None))
        }
    }

    pub(crate) fn plan_reassert_disabled(&mut self) -> Option<ImeCommand> {
        if self.enabled {
            return None;
        }
        Some(ImeCommand::Associate(None))
    }

    pub(crate) fn plan_release_before_destroy(&mut self) -> Option<ImeCommand> {
        self.enabled = true;
        self.clear_pending();
        self.composing = false;
        self.saved_context
            .take()
            .map(|context| ImeCommand::Associate(Some(context)))
    }

    pub(crate) fn plan_cursor_area(&mut self, area: Option<PhysicalRect>) -> Option<ImeCommand> {
        self.cursor_area = area;
        area.map(ImeCommand::SetCaret)
    }

    pub(crate) fn store_detached(&mut self, detached: Option<HIMC>) {
        self.saved_context = detached;
    }

    pub(crate) fn store_reasserted(&mut self, detached: Option<HIMC>) {
        if self.saved_context.is_none() {
            self.saved_context = detached;
        }
    }

    pub(crate) fn note_applied(&mut self, applied: ImeApplied) {
        match applied {
            ImeApplied::Done => {}
            ImeApplied::Detached(context) => self.store_detached(context),
            ImeApplied::RestoreFailed => {
                self.enabled = false;
            }
        }
    }

    pub(crate) fn note_reasserted(&mut self, applied: ImeApplied) {
        match applied {
            ImeApplied::Detached(context) => self.store_reasserted(context),
            other => self.note_applied(other),
        }
    }
}

pub(crate) fn execute(hwnd: HWND, command: ImeCommand) -> ImeApplied {
    unsafe {
        match command {
            ImeCommand::Associate(context) => {
                let previous = ImmAssociateContext(hwnd, context.unwrap_or_default());
                if context.is_none() {
                    ImeApplied::Detached((!previous.is_invalid()).then_some(previous))
                } else {
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

pub(crate) fn mask_setcontext(wparam: usize, lparam: isize) -> isize {
    if wparam == 0 {
        return lparam;
    }
    let flags = lparam as u32;
    let masked = flags & !(ISC_SHOWUICOMPOSITIONWINDOW | ISC_SHOWUIGUIDELINE);
    masked as isize
}

struct CompositionRead {
    text: SmolStr,
    caret: Option<u32>,
    clauses: ClauseRanges,
}

fn composition_events(
    flags: u32,
    committed: Option<SmolStr>,
    read: Option<CompositionRead>,
) -> SmallVec<[ImeEvent; 2]> {
    let mut events = SmallVec::new();
    if flags & GCS_RESULTSTR.0 != 0
        && let Some(text) = committed
    {
        events.push(ImeEvent::Commit {
            text,
            replace: None,
        });
    }
    if flags & GCS_COMPSTR.0 != 0
        && let Some(read) = read
    {
        events.push(ImeEvent::Preedit {
            text: read.text,
            caret: read.caret,
            clauses: read.clauses,
            replace: None,
        });
    }
    events
}

fn read_composition(hwnd: HWND, flags: u32) -> Option<CompositionRead> {
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
        let len = text.encode_utf16().count();
        let clauses = preedit_clauses(
            attrs.as_deref(),
            clause_offsets.as_deref(),
            len,
            ATTR_INPUT as u8,
        );
        let caret = caret.map(|position| clamp_caret(position, &text));
        Some(CompositionRead {
            text,
            caret,
            clauses,
        })
    }
}

fn clamp_caret(position: u32, text: &str) -> u32 {
    let len = text.encode_utf16().count();
    let mut caret = (position as usize).min(len);
    if caret > 0 && caret < len && text.encode_utf16().nth(caret).is_some_and(is_low_surrogate) {
        caret -= 1;
    }
    caret as u32
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

fn cursor_position(himc: HIMC) -> Option<u32> {
    let position = unsafe { ImmGetCompositionStringW(himc, GCS_CURSORPOS, None, 0) };
    (position >= 0).then_some(position as u32)
}

fn composition_string(
    hwnd: HWND,
    flag: windows::Win32::UI::Input::Ime::IME_COMPOSITION_STRING,
) -> Option<SmolStr> {
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

fn composition_string_with(
    himc: HIMC,
    flag: windows::Win32::UI::Input::Ime::IME_COMPOSITION_STRING,
) -> Option<SmolStr> {
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

fn composition_bytes(
    himc: HIMC,
    flag: windows::Win32::UI::Input::Ime::IME_COMPOSITION_STRING,
) -> Vec<u8> {
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

fn composition_dwords(
    himc: HIMC,
    flag: windows::Win32::UI::Input::Ime::IME_COMPOSITION_STRING,
) -> Vec<u32> {
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

fn set_caret_forms(hwnd: HWND, area: PhysicalRect) {
    let (composition, candidate) = caret_forms(area);
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
    use smol_str::SmolStr;
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::Ime::{
                CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, GCS_COMPSTR, GCS_RESULTSTR,
                HIMC, ISC_SHOWUICANDIDATEWINDOW, ISC_SHOWUICOMPOSITIONWINDOW, ISC_SHOWUIGUIDELINE,
                ImmGetCandidateWindow, ImmGetCompositionWindow, ImmGetContext, ImmReleaseContext,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WINDOW_EX_STYLE,
                WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION, WNDCLASSW, WS_OVERLAPPEDWINDOW,
            },
        },
    };

    use super::{
        CompositionRead, ImeApplied, ImeCommand, ImeState, PENDING_COMMIT_WINDOW_MS, caret_forms,
        clamp_caret, composition_events, execute, mask_setcontext,
    };
    use crate::{event::ImeEvent, math::PhysicalRect, platform::ime::ClauseRanges};

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn hidden_window(tag: &str) -> (HWND, Vec<u16>) {
        let class_name = wide(tag);
        let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&class) };
        assert_ne!(atom, 0, "RegisterClassW");
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

    fn drive(state: &mut ImeState, hwnd: HWND, command: Option<ImeCommand>) {
        if let Some(command) = command {
            let applied = execute(hwnd, command);
            state.note_applied(applied);
        }
    }

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

    #[test]
    fn setcontext_masking_leaves_the_inactive_notification_alone() {
        let lparam = ISC_SHOWUICOMPOSITIONWINDOW as isize;
        assert_eq!(mask_setcontext(0, lparam), lparam);
        assert_eq!(mask_setcontext(1, 0), 0);
    }

    #[test]
    fn committed_astral_text_queues_one_unit_per_utf16_code_unit() {
        let mut state = ImeState::default();
        state.queue_commit("🌍a", 0);
        assert_eq!(state.pending_chars.len(), 3, "🌍a = 高代理 + 低代理 + 'a'");

        assert!(state.consume_char(0xD83C, 0));
        assert!(state.consume_char(0xDF0D, 0));
        assert!(state.consume_char(u16::from(b'a'), 0));
        assert!(
            !state.consume_char(u16::from(b'b'), 0),
            "队列清空后必须放行真实键入"
        );
    }

    #[test]
    fn a_mismatched_char_clears_the_queue_instead_of_swallowing_later_input() {
        let mut state = ImeState::default();
        state.queue_commit("你好", 0);

        assert!(!state.consume_char(u16::from(b'x'), 0));
        assert!(
            !state.consume_char(u16::from(b'y'), 0),
            "残留队列必须已清空"
        );
    }

    #[test]
    fn a_commit_only_suppresses_chars_in_the_following_message_batch() {
        let mut state = ImeState::default();
        state.queue_commit("a", 0);
        assert!(state.consume_char(u16::from(b'a'), 0), "同批补发被抑制");

        state.queue_commit("a", 0);
        state.end_message_batch();
        assert!(
            state.consume_char(u16::from(b'a'), 0),
            "PostMessage 补发的字符可能落在下一次消息排空里"
        );

        state.queue_commit("a", 0);
        state.end_message_batch();
        state.end_message_batch();
        assert!(
            !state.consume_char(u16::from(b'a'), 0),
            "超过『紧随其后的一轮』的残留不得吞掉真实键入"
        );
        assert!(state.pending_chars.is_empty(), "过期队列必须被清空");
    }

    #[test]
    fn a_commit_queue_expires_outside_the_message_time_window() {
        let mut state = ImeState::default();
        let committed_at = 1_000;
        state.queue_commit("a", committed_at);
        assert!(
            !state.consume_char(u16::from(b'a'), committed_at + PENDING_COMMIT_WINDOW_MS + 1),
            "时间窗之外不是补发字符"
        );
        assert!(state.pending_chars.is_empty());
        state.queue_commit("b", u32::MAX - 5);
        assert!(
            state.consume_char(u16::from(b'b'), 3),
            "wrapping 时间差（u32::MAX-5 → 3）只有 9 ms"
        );
    }

    #[test]
    fn a_second_commit_in_the_same_batch_appends_to_the_queue() {
        let mut state = ImeState::default();
        state.queue_commit("a", 0);
        state.queue_commit("b", 0);
        assert!(
            state.consume_char(u16::from(b'a'), 0),
            "第一次提交的字符不能丢"
        );
        assert!(state.consume_char(u16::from(b'b'), 0));
        assert!(state.pending_chars.is_empty());
    }

    #[test]
    fn disabling_the_ime_drops_the_pending_characters() {
        let mut state = ImeState::default();
        state.queue_commit("ni", 0);

        let command = state.plan_enabled(false);

        assert!(
            matches!(command, Some(ImeCommand::Associate(None))),
            "禁用必须产出摘除上下文的命令"
        );
        assert!(state.pending_chars.is_empty(), "禁用必须清空待抑制队列");
        assert!(!state.enabled());
    }

    #[test]
    fn the_composition_caret_is_clamped_to_the_composition_length() {
        let text = "abc";
        assert_eq!(clamp_caret(0, text), 0);
        assert_eq!(clamp_caret(2, text), 2);
        assert_eq!(clamp_caret(3, text), 3);
        assert_eq!(clamp_caret(99, text), 3, "越界值退化为末尾");
        assert_eq!(clamp_caret(u32::MAX, ""), 0);
    }

    #[test]
    fn the_composition_caret_is_snapped_out_of_surrogate_pairs() {
        let text = "🌍a";
        let expected = [(0, 0), (1, 0), (2, 2), (3, 3), (4, 3), (99, 3)];
        for (raw, want) in expected {
            assert_eq!(clamp_caret(raw, text), want, "caret {raw} → {want}");
        }
        assert_eq!(clamp_caret(2, text), 2);
        assert_eq!(clamp_caret(1, "🌍"), 0);
        assert_eq!(clamp_caret(2, "🌍"), 2);
    }

    #[test]
    fn reassert_is_a_no_op_while_the_ime_is_enabled() {
        let mut state = ImeState::default();
        assert!(state.plan_reassert_disabled().is_none());
    }

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
        let outcome = state.handle(
            HWND::default(),
            windows::Win32::UI::WindowsAndMessaging::WM_PAINT,
            0,
            0,
        );

        assert!(outcome.events.is_empty());
        assert!(outcome.command.is_none());
    }

    #[test]
    fn both_result_and_composition_flags_emit_commit_then_preedit() {
        let read = CompositionRead {
            text: SmolStr::new("ni"),
            caret: Some(2),
            clauses: ClauseRanges::new(),
        };
        let events = composition_events(
            GCS_RESULTSTR.0 | GCS_COMPSTR.0,
            Some(SmolStr::new("你")),
            Some(read),
        );

        assert_eq!(events.len(), 2, "两个 flag 必须各产一条事件：{events:?}");
        assert_eq!(
            events[0],
            ImeEvent::Commit {
                text: SmolStr::new("你"),
                replace: None
            },
            "先提交（XML 求值顺序第 3 步）"
        );
        assert!(
            matches!(&events[1], ImeEvent::Preedit { text, caret: Some(2), .. } if text == "ni"),
            "再交付新组合串：{:?}",
            events[1]
        );
    }

    #[test]
    fn a_missing_commit_still_delivers_the_new_preedit() {
        let read = CompositionRead {
            text: SmolStr::new("hao"),
            caret: None,
            clauses: ClauseRanges::new(),
        };
        let events = composition_events(GCS_RESULTSTR.0 | GCS_COMPSTR.0, None, Some(read));

        assert!(
            matches!(events.as_slice(), [ImeEvent::Preedit { .. }]),
            "提交读不到时仍必须交付新组合串：{events:?}"
        );
    }

    #[test]
    fn a_single_flag_still_produces_exactly_one_event() {
        let read = CompositionRead {
            text: SmolStr::new("ni"),
            caret: None,
            clauses: ClauseRanges::new(),
        };
        let commit = composition_events(GCS_RESULTSTR.0, Some(SmolStr::new("你")), Some(read));
        assert!(matches!(commit.as_slice(), [ImeEvent::Commit { .. }]));
        let preedit = composition_events(
            GCS_COMPSTR.0,
            Some(SmolStr::new("你")),
            Some(CompositionRead {
                text: SmolStr::new("ni"),
                caret: None,
                clauses: ClauseRanges::new(),
            }),
        );
        assert!(matches!(preedit.as_slice(), [ImeEvent::Preedit { .. }]));
        assert!(
            composition_events(0, Some(SmolStr::new("你")), None).is_empty(),
            "没有相关 flag 时不得凭已有数据硬造事件"
        );
    }

    #[test]
    fn starting_a_new_composition_drops_the_pending_commit_queue() {
        let mut state = ImeState::default();
        state.queue_commit("ni", 0);
        let outcome = state.handle(HWND::default(), WM_IME_STARTCOMPOSITION, 0, 0);

        assert!(matches!(outcome.events.as_slice(), [ImeEvent::Start]));
        assert!(state.pending_chars.is_empty(), "重新组合必须清空待抑制码元");
        assert!(!state.consume_char(u16::from(b'n'), 0), "不得再吞真实键入");
    }

    #[test]
    fn hidden_window_can_read_back_caret_forms() {
        let (hwnd, _class) = hidden_window("KytrixP16ImeForms");
        let mut state = ImeState::default();
        let command = state.plan_cursor_area(Some(PhysicalRect::new(100, 200, 101, 220)));
        drive(&mut state, hwnd, command);
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

    #[test]
    fn release_before_destroy_restores_the_saved_ime_context() {
        let (hwnd, _class) = hidden_window("KytrixP16ImeRelease");
        let mut state = ImeState::default();
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
            assert!(state.enabled());
            let _ = ImmReleaseContext(hwnd, restored);

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

    #[test]
    fn reasserting_a_disabled_ime_detaches_again_without_overwriting_the_saved_context() {
        let (hwnd, _class) = hidden_window("KytrixP16ImeReassert");
        let mut state = ImeState::default();
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

            let _ = super::ImmAssociateContext(hwnd, saved.expect("saved context"));
            let command = state.plan_reassert_disabled();
            drive_reassert(&mut state, hwnd, command);
            assert!(ImmGetContext(hwnd).is_invalid(), "重新关联后必须再摘一次");
            assert_eq!(state.saved_context, saved, "不得覆盖待恢复的上下文");

            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn restoring_without_a_saved_context_asks_the_system_for_the_default_one() {
        let mut state = ImeState::default();
        state.plan_enabled(false);
        state.saved_context = None;

        let command = state.plan_enabled(true);

        assert!(
            matches!(command, Some(ImeCommand::AssociateDefault)),
            "没有保存值时必须让系统关联默认上下文"
        );
        assert!(state.enabled());
    }

    #[test]
    fn commands_are_plain_data_that_can_outlive_the_borrow() {
        fn takes(_: ImeCommand) {}
        takes(ImeCommand::Associate(Some(HIMC::default())));
        takes(ImeCommand::Associate(None));
        takes(ImeCommand::AssociateDefault);
        takes(ImeCommand::SetCaret(PhysicalRect::new(0, 0, 1, 1)));
    }

    #[test]
    fn a_focus_loss_during_composition_emits_exactly_one_end() {
        let (hwnd, _class) = hidden_window("KytrixImeFocusLoss");
        let mut state = ImeState::default();
        let start = state.handle(hwnd, WM_IME_STARTCOMPOSITION, 0, 0);
        assert!(matches!(start.events.as_slice(), [ImeEvent::Start]));
        assert!(state.is_composing(), "START 后平台必须记为组合中");

        assert_eq!(state.finish_composition(), Some(ImeEvent::End));
        assert!(!state.is_composing(), "补发 End 后组合态必须清零");
        assert_eq!(state.finish_composition(), None, "重复清场不得再补 End");

        let late = state.handle(hwnd, WM_IME_ENDCOMPOSITION, 0, 0);
        assert!(
            late.events.is_empty(),
            "系统迟到的 WM_IME_ENDCOMPOSITION 不得重复投递 End"
        );
    }

    #[test]
    fn a_focus_loss_outside_composition_emits_no_end() {
        let mut state = ImeState::default();
        assert!(!state.is_composing());
        assert_eq!(state.finish_composition(), None);
    }

    #[test]
    fn an_end_composition_without_a_live_composition_emits_nothing() {
        let (hwnd, _class) = hidden_window("KytrixImeStrayEnd");
        let mut state = ImeState::default();
        let outcome = state.handle(hwnd, WM_IME_ENDCOMPOSITION, 0, 0);
        assert!(outcome.events.is_empty());
    }

    #[test]
    fn disabling_the_ime_clears_the_composition_state() {
        let (hwnd, _class) = hidden_window("KytrixImeDisable");
        let mut state = ImeState::default();
        assert_eq!(
            state
                .handle(hwnd, WM_IME_STARTCOMPOSITION, 0, 0)
                .events
                .len(),
            1
        );
        assert!(state.is_composing());

        let command = state.plan_enabled(false);

        assert!(command.is_some());
        assert!(!state.is_composing(), "禁用必须清平台组合态");
        assert_eq!(
            state.finish_composition(),
            None,
            "禁用清场后的失焦不得补过期 End"
        );
    }
}

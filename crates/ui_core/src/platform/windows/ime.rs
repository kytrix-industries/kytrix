//! 最小 IME 通路（`design/phase-09-win32-demo.md` §4.4 / `design/phase-16-ime-caret.md`）。
//!
//! 事件通路负责组合开始/更新/结束与 `GCS_RESULTSTR` 提交；P16 增加 caret 区域到
//! `ImmSetCompositionWindow` + `ImmSetCandidateWindow` 的定位通路，不再使用最后指针位置。
//! probe 实测本机 `ImmGetContext` 返回有效 `HIMC`，两个 Set 函数与对应的 Get 回读均成功。
//!
//! 另有**应用可主动关闭 IME** 的通路（自绘搜索框的英文直输模式）：[`ImeState::set_enabled`]
//! 用 `ImmAssociateContext(hwnd, NULL)` 把输入上下文从窗口上摘下来并保存旧 `HIMC`，
//! 重新启用时还回去；禁用期间 `WndProc` 吞掉全部 `WM_IME_*`（见 `event_loop.rs`）。

use smol_str::SmolStr;
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::{
        Input::Ime::{
            CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, GCS_COMPSTR, GCS_RESULTSTR,
            HIMC, IACE_DEFAULT, IME_COMPOSITION_STRING, ImmAssociateContext, ImmAssociateContextEx,
            ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow,
            ImmSetCompositionWindow,
        },
        WindowsAndMessaging::{WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION},
    },
};

use crate::{event::ImeEvent, math::PhysicalRect};

/// IME 组合状态（每个窗口一份，由 `WndProcState` 持有）。
#[derive(Debug)]
pub(crate) struct ImeState {
    /// 上一次提交的字符数：随后到达的 `WM_CHAR` 要跳过，避免提交两次。
    suppress_chars: usize,
    /// 应用声明的 caret 区域（客户区物理像素）；`None` = 无 hint。
    cursor_area: Option<PhysicalRect>,
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
}

impl Default for ImeState {
    fn default() -> Self {
        Self {
            suppress_chars: 0,
            cursor_area: None,
            enabled: true,
            saved_context: None,
        }
    }
}

impl ImeState {
    /// 处理一条 `WM_IME_*`；`None` = 该消息不产生应用事件。
    pub(crate) fn handle(&mut self, hwnd: HWND, msg: u32, lparam: isize) -> Option<ImeEvent> {
        match msg {
            WM_IME_STARTCOMPOSITION => {
                self.apply_cursor_area(hwnd);
                Some(ImeEvent::Start)
            }
            WM_IME_COMPOSITION => {
                let flags = lparam as u32;
                if flags & GCS_RESULTSTR.0 != 0 {
                    let text = composition_string(hwnd, GCS_RESULTSTR)?;
                    self.suppress_chars = text.chars().count();
                    return Some(ImeEvent::Commit {
                        text,
                        // WIN32 `GCS_RESULTSTR` 就是本次提交的完整串：IMM32 没有 AppKit
                        // `replacementRange` 那种「只替换组合串的一段」的区间信息，
                        // `None` = 替换整个当前组合串（与 `GCS_COMPSTR` 的全量语义一致）。
                        replace: None,
                    });
                }
                if flags & GCS_COMPSTR.0 != 0 {
                    let text = composition_string(hwnd, GCS_COMPSTR).unwrap_or_default();
                    return Some(ImeEvent::Preedit {
                        text,
                        // `GCS_COMPSTR` 是**完整**组合串（不是增量），所以整体替换。
                        replace: None,
                    });
                }
                None
            }
            WM_IME_ENDCOMPOSITION => Some(ImeEvent::End),
            _ => None,
        }
    }

    /// `WM_CHAR` 是否应被 IME 提交吞掉（提交后 Windows 会再发同样的字符）。
    pub(crate) fn take_char_suppression(&mut self) -> bool {
        if self.suppress_chars == 0 {
            return false;
        }
        self.suppress_chars -= 1;
        true
    }

    /// 本窗口的 IME 当前是否启用（`WndProc` 据此吞掉 `WM_IME_*`）。
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// 应用目标状态与已应用状态不同时调用（泵每轮比较一次，P2-7）。
    ///
    /// 禁用：`ImmAssociateContext(hwnd, None)` 把输入上下文从窗口上摘下来。返回的旧 `HIMC`
    /// 由本状态**保存**（它不会再被系统自动释放）；同时清掉未消费的 `WM_CHAR` 抑制计数 ——
    /// 否则重新启用后第一次真实输入会被上一次提交的余数吞掉。
    ///
    /// 启用：把保存的上下文还回去。没有保存过（禁用时窗口本来就没有关联上下文）则用
    /// `ImmAssociateContextEx(..., IACE_DEFAULT)` 让系统给窗口关联**默认**上下文。
    ///
    /// 覆盖情况：`wndproc_tests` 的隐藏窗口用例实际只走**保存/恢复**分支（实测隐藏窗口创建时就有
    /// 默认 HIMC，`ImmAssociateContext` 返回非 NULL）；`saved_context == None` 的
    /// `IACE_DEFAULT` 分支只在「禁用时窗口本来无上下文」时出现，暂无自动测试（真机 IME 可覆盖）。
    pub(crate) fn set_enabled(&mut self, hwnd: HWND, enabled: bool) {
        if enabled == self.enabled {
            return;
        }
        // 先记账再动 IMM32：`ImmAssociateContext*` 会**同步**派发 `WM_IME_SETCONTEXT`
        // （wParam = 上下文激活/失活），嵌套进入的 `WndProc` 必须看到新状态 —— 禁用期间
        // 那条消息要被吞掉，否则默认过程可能把组合窗重新显示出来。
        self.enabled = enabled;
        // SAFETY: `hwnd` 由调用方保证是存活窗口；两个 API 只改「窗口 ↔ 输入上下文」的关联，
        // 不持有 Rust 引用。`saved_context` 里的 HIMC 在恢复时原样还回，不二次释放。
        unsafe {
            if enabled {
                match self.saved_context.take() {
                    Some(context) => {
                        // 返回值是「被替换的旧上下文」：窗口刚被摘下，正常为 NULL；
                        // 若禁用期间系统又给窗口关联了新上下文（焦点/输入法切换），它会在这里
                        // 被替换出来，随后随窗口释放，不需要本层释放（复审 W1-3）。
                        let _ = ImmAssociateContext(hwnd, context);
                    }
                    None => {
                        let restored = ImmAssociateContextEx(hwnd, HIMC::default(), IACE_DEFAULT);
                        if !restored.as_bool() {
                            // 恢复失败：回滚成「禁用」，下一轮泵会重试；否则 `enabled` 已记账、
                            // 永远不会再试 ⇒ IME 静默失效（复审 W1-4）。
                            self.enabled = false;
                        }
                    }
                }
            } else {
                let previous = ImmAssociateContext(hwnd, HIMC::default());
                self.saved_context = (!previous.is_invalid()).then_some(previous);
                self.suppress_chars = 0;
            }
        }
    }

    /// 窗口销毁**之前**把摘下来的上下文还回去，让系统随窗口释放它（避免 `HIMC` 泄漏）。
    ///
    /// 必须在 `DestroyWindow` 之前、HWND 仍有效时调用。没有保存过上下文则无事可做：
    /// 禁用时窗口本来就没有上下文，不存在本层拥有的句柄。
    pub(crate) fn release_before_destroy(&mut self, hwnd: HWND) {
        if let Some(context) = self.saved_context.take() {
            // SAFETY: 同 `set_enabled`；`hwnd` 仍是存活窗口（调用方保证在 DestroyWindow 之前）。
            unsafe {
                let _ = ImmAssociateContext(hwnd, context);
            }
        }
        self.enabled = true;
    }

    /// 保存并立刻应用 caret 区域（客户区物理像素）。
    pub(crate) fn set_cursor_area(&mut self, hwnd: HWND, area: PhysicalRect) {
        self.cursor_area = Some(area);
        self.apply_cursor_area(hwnd);
    }

    /// 清除 hint；下一次 `WM_IME_STARTCOMPOSITION` 不再重放位置。
    pub(crate) fn clear_cursor_area(&mut self) {
        self.cursor_area = None;
    }

    /// 把已保存的 caret 区域写给 IMM32（组合窗 + 候选窗）。
    fn apply_cursor_area(&self, hwnd: HWND) {
        let Some(area) = self.cursor_area else {
            return;
        };
        let (composition, candidate) = caret_forms(area);
        // SAFETY: `himc` 由 `ImmGetContext` 取得、用后立即释放；两个 form 都是栈上值。
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
}

/// caret 区域 → `(COMPOSITIONFORM, CANDIDATEFORM)`。
///
/// - 组合窗落点：caret 区域底边（`CFS_POINT`）；
/// - 候选窗：左上角 + `CFS_EXCLUDE`，让 IME 避开文字区域。
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

/// 取组合串（UTF-16 → `SmolStr`）；上下文无效或长度 ≤ 0 时返回 `None`。
fn composition_string(hwnd: HWND, flag: IME_COMPOSITION_STRING) -> Option<SmolStr> {
    // SAFETY: `himc` 成对释放；缓冲区由本函数独占，`written` 之后才读。
    unsafe {
        let himc: HIMC = ImmGetContext(hwnd);
        if himc.is_invalid() {
            return None;
        }
        let bytes = ImmGetCompositionStringW(himc, flag, None, 0);
        if bytes <= 0 {
            let _ = ImmReleaseContext(hwnd, himc);
            return None;
        }
        let mut units = vec![0u16; (bytes as usize).div_ceil(2)];
        let written = ImmGetCompositionStringW(
            himc,
            flag,
            Some(units.as_mut_ptr().cast()),
            (units.len() * 2) as u32,
        );
        let _ = ImmReleaseContext(hwnd, himc);
        if written <= 0 {
            return None;
        }
        let len = written as usize / 2;
        Some(SmolStr::new(String::from_utf16_lossy(&units[..len])))
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::Ime::{
                CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, ImmGetCandidateWindow,
                ImmGetCompositionWindow, ImmGetContext, ImmReleaseContext,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WINDOW_EX_STYLE,
                WNDCLASSW, WS_OVERLAPPEDWINDOW,
            },
        },
    };

    use super::{ImeState, caret_forms};
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
    fn hidden_window_can_read_back_caret_forms() {
        let (hwnd, _class) = hidden_window("KytrixP16ImeForms");
        let mut state = ImeState::default();
        state.set_cursor_area(hwnd, PhysicalRect::new(100, 200, 101, 220));
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

    #[test]
    fn committed_text_suppresses_the_matching_wm_chars() {
        let mut state = ImeState {
            suppress_chars: 3,
            ..ImeState::default()
        };

        assert!(state.take_char_suppression());
        assert!(state.take_char_suppression());
        assert!(state.take_char_suppression());
        assert!(!state.take_char_suppression(), "只有提交过的字符会被吞");
    }

    #[test]
    fn unknown_messages_do_not_produce_ime_events() {
        let mut state = ImeState::default();
        // SAFETY: 该消息分支不读 `hwnd`；传空句柄是安全的（只走 `_` 分支）。
        let event = state.handle(
            windows::Win32::Foundation::HWND::default(),
            windows::Win32::UI::WindowsAndMessaging::WM_PAINT,
            0,
        );

        assert!(event.is_none());
    }
}

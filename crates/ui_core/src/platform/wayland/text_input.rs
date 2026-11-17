//! `zwp_text_input_v3` 的客户端侧纯逻辑：焦点路由（R10-6 / R10-13）、无输入法守护进程时的
//! 键盘回退（R10-2）、`content_type` 映射（R10-12 的安全部分）、修饰键的文本抑制（R10-5）。
//!
//! # 为什么 IME 不能「一 enable 就永久静默键盘通路」
//!
//! `zwp_text_input_v3` **没有**按键转发，也没有「这个键被输入法消费了」的回报：`preedit_string`
//! / `commit_string` 是双缓冲的**状态**，只在 `done` 时生效。因此客户端无法知道某个
//! `wl_keyboard.key` 最终会不会变成文本。旧实现把「IME 已 enable」当作「文本走 IME」的充分
//! 条件，于是：
//!
//! - **没有输入法守护进程**（sway / Hyprland 默认安装，很常见）⇒ compositor 照样接受 `enable`
//!   但永远不发 `commit_string` ⇒ 字符输入全灭（只剩 Esc/Tab/方向键）。这是交易终端的致命故障；
//! - 反过来若 IM 只在组合态提交，普通 ASCII 也可能丢失。
//!
//! 本模块的判据（GTK 的 `GtkIMContextSimple` 回退、Chromium 的 composition-only 抑制的工程折中）：
//!
//! ```text
//! 文本走 IME  ⟺  焦点 surface 已 enable  ∧  未被降级
//! 降级        =   enable 之后 NO_IM_FALLBACK 内**没有任何** IM 事件（preedit/commit/delete）
//! 升级        =  降级后收到任何 IM 事件 ⇒ 立刻交回 IME
//! ```
//!
//! 「无任何 IM 事件」是唯一可观测的存活信号，所以降级必须是**启发式 + 可逆**的。
//!
//! # `done` 的 serial（R10-13）
//!
//! XML 原文：*"The value of the serial argument must be equal to the number of commit requests
//! already issued on that object. When the client receives a done event with a serial different
//! … it must proceed with evaluating and applying the changes as normal, except it should not
//! change the current state of the zwp_text_input_v3 object. All pending state requests
//! (`set_surrounding_text`, `set_content_type`, `set_cursor_rectangle`) … should be sent and
//! committed after receiving a `done` event with a matching serial."*
//!
//! 即：文本变化**总是**应用；只有「我们自己发的状态」要等 serial 对上再发。本状态机因此把
//! 两件事分开：`done` 的文本路由恒走 [`TextInputRoute::focus`]，状态刷新由
//! [`TextInputRoute::settle`] 的返回值决定。

use std::time::{Duration, Instant};

use crate::{event::Modifiers, window::ImeContentType};

/// `enable()` 之后等待「本 seat 有输入法引擎」证据的窗口。
///
/// 1.5 s 与 GTK 的回退节奏同量级：正常 IM 在 enable 后几十毫秒内就会发出第一批
/// `preedit_string`；而人类开始打字前通常已经有 100 ms 以上。
pub(crate) const NO_IM_FALLBACK: Duration = Duration::from_millis(1500);

/// 一次 text-input 焦点变化要下发的协议动作（`surface.rs` 据此驱动真实对象）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FocusChange {
    /// 先对**旧的**焦点 surface 发 `disable()`。
    ///
    /// XML `enable`：`must disable the current text input before enabling the new one`。
    pub disable: bool,
    /// 再对新的焦点 surface 发 `enable()` + 状态 + `commit()`。
    pub enable: bool,
}

/// `zwp_text_input_v3` 的客户端侧状态机（纯逻辑，可在开发机上单测）。
#[derive(Debug, Default)]
pub(crate) struct TextInputRoute {
    /// 当前 text-input 焦点（窗口下标；`None` = 没有 enable 任何 surface）。
    focus: Option<u32>,
    /// 已发出的 `commit()` 次数（`done.serial` 的期望值）。
    commits: u32,
    /// 本 seat 是否观察到过 IM 事件（preedit / commit / delete_surrounding）。
    saw_im_event: bool,
    /// 无 IM 守护进程的降级（键盘文本通路接管）。
    degraded: bool,
    /// `enable()` 的时刻（降级计时起点）。
    enabled_at: Option<Instant>,
}

impl TextInputRoute {
    /// `zwp_text_input_v3.enter`（或 `wl_keyboard.enter` 的 IME 侧）：把焦点设到 `window`。
    pub(crate) fn enter(&mut self, window: u32, now: Instant) -> FocusChange {
        if self.focus == Some(window) {
            return FocusChange::default();
        }
        let change = FocusChange {
            disable: self.focus.is_some(),
            enable: true,
        };
        self.focus = Some(window);
        // 新目标重新判定存活：上一次降级不影响新窗口（不同的 surface 可能挂不同输入法）。
        self.saw_im_event = false;
        self.degraded = false;
        self.enabled_at = Some(now);
        change
    }

    /// 关闭窗口 / 退出：无条件停用（`remove_window` 不能再误伤焦点窗口，见 `is_focus`）。
    pub(crate) fn disable(&mut self) -> FocusChange {
        if self.focus.is_none() {
            return FocusChange::default();
        }
        self.focus = None;
        self.degraded = false;
        self.saw_im_event = false;
        self.enabled_at = None;
        FocusChange {
            disable: true,
            enable: false,
        }
    }

    /// 当前 text-input 焦点窗口（`done` 的事件路由依据）。
    #[must_use]
    pub(crate) fn focus(&self) -> Option<u32> {
        self.focus
    }

    /// 该窗口是否正是当前焦点（`remove_window` 只应清理它，不能误关别人的 IME）。
    #[must_use]
    pub(crate) fn is_focus(&self, window: u32) -> bool {
        self.focus == Some(window)
    }

    /// 一个窗口被移除后修正焦点下标（`windows` 是 `Vec`，移除位置之后的下标整体 -1）。
    ///
    /// 被移除的正是焦点时置 `None`（调用方在此之前已经 `disable` 过协议对象）。
    pub(crate) fn reindex_removed(&mut self, removed: u32) {
        let Some(focus) = self.focus else {
            return;
        };
        if focus == removed {
            self.focus = None;
            self.enabled_at = None;
        } else if focus > removed {
            self.focus = Some(focus - 1);
        }
    }

    /// 记录一次 `commit()` 请求（`done.serial` 的期望值 = 已发出的 commit 次数）。
    pub(crate) fn note_commit(&mut self) {
        self.commits = self.commits.saturating_add(1);
    }

    /// `done.serial` 是否与本对象已发出的 `commit()` 次数一致。
    ///
    /// `true` ⇒ 可以下发上一轮攒下的状态（`set_content_type` / `set_cursor_rectangle` 等）。
    /// `false` ⇒ 按协议**仍然应用文本变化**，但不要下发新状态（会与 compositor 的已知状态错位）。
    #[must_use]
    pub(crate) fn settle(&mut self, serial: u32) -> bool {
        serial == self.commits
    }

    /// 收到任何 IM 事件：证明本 seat 有输入法引擎 ⇒ 撤销降级。
    pub(crate) fn note_im_event(&mut self) {
        self.saw_im_event = true;
        self.degraded = false;
    }

    /// 文本是否**只**走 IME（= `should_emit_text` 的 `ime_takes_text` 参数）。
    ///
    /// `false` 时键盘 xkb 通路必须继续产生 [`crate::event::InputEvent::Text`]。
    #[must_use]
    pub(crate) fn takes_text(&self) -> bool {
        self.focus.is_some() && !self.degraded
    }

    /// 到期降级：返回 `true` 表示**本次**判定降级（一次性，之后 `takes_text` 恒 `false`）。
    pub(crate) fn should_degrade(&mut self, now: Instant) -> bool {
        if self.degraded || self.saw_im_event {
            return false;
        }
        let Some(enabled_at) = self.enabled_at else {
            return false;
        };
        if now.duration_since(enabled_at) < NO_IM_FALLBACK {
            return false;
        }
        self.degraded = true;
        true
    }

    /// 距离降级判定还有多久（事件循环据此压缩 `dispatch` 的等待；`None` = 不需要等）。
    #[must_use]
    pub(crate) fn next_degrade_in(&self, now: Instant) -> Option<Duration> {
        if self.degraded || self.saw_im_event {
            return None;
        }
        let enabled_at = self.enabled_at?;
        Some(NO_IM_FALLBACK.saturating_sub(now.duration_since(enabled_at)))
    }
}

// ── `content_type` 映射（R10-12）──────────────────────────────────────────

/// `zwp_text_input_v3.content_hint` 的位值（协议原文的枚举值）。
pub(crate) const CONTENT_HINT_HIDDEN_TEXT: u32 = 0x40;
pub(crate) const CONTENT_HINT_SENSITIVE_DATA: u32 = 0x80;

/// `set_content_type` 的两个参数（协议里是 uint 位的 hint + 枚举码 purpose）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentTypeBits {
    /// `content_hint` 位域。
    pub hint: u32,
    /// `content_purpose` 枚举码。
    pub purpose: u32,
}

/// 引擎的 [`ImeContentType`] → `(content_hint, content_purpose)`。
///
/// 密码 / PIN 必须同时给 `hidden_text`（不在界面上明文回显组合串）与 `sensitive_data`
/// （输入法**不得**把内容存进用户词库 / 联想库 / 同步到云端）。这是安全问题：交易终端的
/// API key 被写进输入法词库等于泄露。
#[must_use]
pub(crate) fn content_type_bits(content: ImeContentType) -> ContentTypeBits {
    match content {
        ImeContentType::Text => ContentTypeBits {
            hint: 0,
            purpose: 0,
        },
        ImeContentType::Password => ContentTypeBits {
            hint: CONTENT_HINT_HIDDEN_TEXT | CONTENT_HINT_SENSITIVE_DATA,
            purpose: 8,
        },
        ImeContentType::Pin => ContentTypeBits {
            hint: CONTENT_HINT_HIDDEN_TEXT | CONTENT_HINT_SENSITIVE_DATA,
            purpose: 9,
        },
        // 金额 / 数量：允许小数点与符号，但不隐藏、不敏感（用户看得见自己在输什么）。
        ImeContentType::Number => ContentTypeBits {
            hint: 0,
            purpose: 3,
        },
    }
}

// ── 修饰键的文本抑制（R10-5）──────────────────────────────────────────────

/// 修饰键组合是否接管了本次按键 ⇒ **不得**产生字符文本。
///
/// `xkb_state.key_get_utf8` 按**当前修饰键状态**取字符，而 Ctrl/Alt/Super 通常不改变 shift
/// level：`Ctrl+C` 会拿到 `"c"`、`Alt+F` 拿到 `"f"`、`Super+R` 拿到 `"r"`。
/// [`crate::event::InputEvent::Text`] 不携带修饰键，App 无从过滤 ⇒ 焦点在输入框时按
/// `Ctrl+C/V/A/W` 会把字母打进文本框，随后快捷键再执行一次（「复制时输入框多一个 c」）。
///
/// **不过滤 SHIFT**（大写字母是正常输入）；`AltGr` 在 xkb 里是 `Mod5`，
/// [`Modifiers`] 没有对应位，因此欧洲布局用 AltGr 输入 `@ € { }` 的路径不受影响。
/// GTK 的 `GtkIMContextSimple` 判据与此完全一致（`GDK_CONTROL_MASK|GDK_ALT_MASK|
/// GDK_SUPER_MASK`），winit 的 `KeyEvent::text` 同理。
#[must_use]
pub(crate) fn text_blocked_by_modifiers(modifiers: Modifiers) -> bool {
    modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::META)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        CONTENT_HINT_HIDDEN_TEXT, CONTENT_HINT_SENSITIVE_DATA, NO_IM_FALLBACK, TextInputRoute,
        content_type_bits, text_blocked_by_modifiers,
    };
    use crate::{event::Modifiers, window::ImeContentType};

    fn route_with_focus(now: Instant) -> TextInputRoute {
        let mut route = TextInputRoute::default();
        let change = route.enter(7, now);
        assert!(change.enable && !change.disable);
        route.note_commit();
        route
    }

    /// R10-6：焦点切换必须先 disable 旧的再 enable 新的（XML `enable` 的硬要求）。
    #[test]
    fn focus_changes_disable_the_previous_surface_first() {
        let now = Instant::now();
        let mut route = TextInputRoute::default();
        assert_eq!(
            route.enter(1, now),
            super::FocusChange {
                disable: false,
                enable: true
            }
        );
        assert_eq!(route.focus(), Some(1));
        assert_eq!(
            route.enter(2, now),
            super::FocusChange {
                disable: true,
                enable: true
            }
        );
        assert_eq!(route.focus(), Some(2));
    }

    #[test]
    fn entering_the_same_surface_twice_is_a_no_op() {
        let now = Instant::now();
        let mut route = TextInputRoute::default();
        route.enter(3, now);
        assert_eq!(route.enter(3, now), super::FocusChange::default());
    }

    /// R10-6 的核心回归：`is_focus` 是「该不该为这个窗口停用 IME」的唯一判据 —— 关闭/失焦
    /// **非焦点**窗口时平台层不得动焦点窗口的 IME。
    #[test]
    fn only_the_focused_window_is_reported_as_the_ime_target() {
        let now = Instant::now();
        let mut route = TextInputRoute::default();
        assert_eq!(
            route.enter(5, now),
            super::FocusChange {
                disable: false,
                enable: true
            }
        );
        assert!(!route.is_focus(4), "别的窗口离开时不得停用当前 IME");
        assert!(route.is_focus(5));
        assert_eq!(route.focus(), Some(5), "焦点窗口的 IME 必须继续有效");
        assert_eq!(
            route.disable(),
            super::FocusChange {
                disable: true,
                enable: false
            }
        );
        assert_eq!(route.focus(), None);
        assert!(!route.is_focus(5), "停用之后不再报告为焦点");
    }

    /// `remove_window` 只在被移除窗口就是焦点时才停用。
    #[test]
    fn disable_is_idempotent_and_reports_whether_it_changed_anything() {
        let now = Instant::now();
        let mut route = TextInputRoute::default();
        assert_eq!(route.disable(), super::FocusChange::default());
        route.enter(1, now);
        assert_eq!(
            route.disable(),
            super::FocusChange {
                disable: true,
                enable: false
            }
        );
        assert_eq!(route.disable(), super::FocusChange::default());
    }

    /// R10-6：窗口被移除后焦点下标必须跟着前移（否则 IME 会投给另一个窗口）。
    #[test]
    fn removing_a_window_reindexes_the_ime_target() {
        let now = Instant::now();
        let mut route = TextInputRoute::default();
        route.enter(3, now);
        // 移除下标 1（在焦点之前）⇒ 焦点 3 → 2。
        route.reindex_removed(1);
        assert_eq!(route.focus(), Some(2));
        // 移除焦点窗口 ⇒ 没有焦点（调用方负责在此之前 disable 协议对象）。
        route.reindex_removed(2);
        assert_eq!(route.focus(), None);
        // 移除之后的窗口不影响焦点。
        route.enter(1, now);
        route.reindex_removed(5);
        assert_eq!(route.focus(), Some(1));
    }

    /// R10-13：`done` 的文本恒应用，但只有 serial 对上才允许下发新状态。
    #[test]
    fn only_a_matching_done_serial_allows_new_state_to_be_sent() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        assert!(route.settle(1), "commit 次数 = 1 ⇒ serial 1 匹配");
        route.note_commit();
        assert!(route.settle(2));
        assert!(
            !route.settle(1),
            "过期的 serial ⇒ 文本照常应用，但不得下发 set_content_type 等状态"
        );
        assert!(!route.settle(3), "未来的 serial 同样不匹配");
    }

    /// R10-2：无 IM 守护进程时必须在 NO_IM_FALLBACK 后把文本交回键盘通路。
    #[test]
    fn a_silent_seat_degrades_to_the_keyboard_text_path() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        assert!(
            route.takes_text(),
            "enable 之后先由 IME 接管文本（最多给 1.5 s）"
        );

        assert!(
            !route.should_degrade(now + NO_IM_FALLBACK - Duration::from_millis(1)),
            "未到窗口期不得降级"
        );
        assert_eq!(
            route.next_degrade_in(now),
            Some(NO_IM_FALLBACK),
            "事件循环需要知道还要等多久"
        );

        assert!(route.should_degrade(now + NO_IM_FALLBACK), "到期降级");
        assert!(!route.takes_text(), "降级后键盘通路必须产生字符");
        assert!(
            !route.should_degrade(now + NO_IM_FALLBACK * 2),
            "降级是一次性事件"
        );
        assert_eq!(route.next_degrade_in(now + NO_IM_FALLBACK), None);
    }

    /// 收到任何 IM 事件 ⇒ 证明有输入法引擎 ⇒ 立刻撤销降级（并且不再计时）。
    #[test]
    fn an_im_event_upgrades_back_from_the_fallback() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        route.note_im_event();
        assert!(
            !route.should_degrade(now + NO_IM_FALLBACK * 3),
            "有 IM 证据时永不降级"
        );
        assert!(route.takes_text());
        assert_eq!(route.next_degrade_in(now), None);
    }

    /// 先降级、随后 IM 才发出事件（慢启动的输入法）：必须能升回来。
    #[test]
    fn a_late_im_event_restores_ime_ownership() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        assert!(route.should_degrade(now + NO_IM_FALLBACK));
        assert!(!route.takes_text());
        route.note_im_event();
        assert!(route.takes_text(), "IM 一旦说话就必须重新独占文本通路");
    }

    /// 换焦点重新计时：新 surface 的 IM 可能完全不同（不同窗口可能挂不同输入法）。
    #[test]
    fn switching_focus_restarts_the_fallback_timer() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        assert!(route.should_degrade(now + NO_IM_FALLBACK));
        assert!(!route.takes_text(), "降级后键盘通路接管");
        route.note_commit();
        let later = now + NO_IM_FALLBACK * 2;
        route.enter(8, later);
        assert!(route.takes_text(), "换焦点后重新给新目标的 IME 一个窗口期");
        assert!(!route.should_degrade(later + Duration::from_millis(10)));
        assert!(route.should_degrade(later + NO_IM_FALLBACK));
    }

    #[test]
    fn no_focus_means_the_keyboard_path_always_owns_text() {
        let route = TextInputRoute::default();
        assert!(!route.takes_text());
        assert_eq!(route.focus(), None);
        assert_eq!(route.next_degrade_in(Instant::now()), None);
    }

    /// R10-12：密码 / PIN 必须同时打开两个 hint（安全语义，不能只设 purpose）。
    #[test]
    fn sensitive_content_types_set_both_hints() {
        let password = content_type_bits(ImeContentType::Password);
        assert_eq!(password.purpose, 8, "content_purpose = password");
        assert_eq!(
            password.hint,
            CONTENT_HINT_HIDDEN_TEXT | CONTENT_HINT_SENSITIVE_DATA
        );

        let pin = content_type_bits(ImeContentType::Pin);
        assert_eq!(pin.purpose, 9, "content_purpose = pin");
        assert_eq!(
            pin.hint,
            CONTENT_HINT_HIDDEN_TEXT | CONTENT_HINT_SENSITIVE_DATA
        );

        assert_eq!(CONTENT_HINT_HIDDEN_TEXT, 0x40);
        assert_eq!(CONTENT_HINT_SENSITIVE_DATA, 0x80);
    }

    #[test]
    fn plain_and_numeric_content_types_are_not_sensitive() {
        assert_eq!(
            content_type_bits(ImeContentType::Text),
            super::ContentTypeBits {
                hint: 0,
                purpose: 0
            },
            "默认值就是 normal + 无 hint"
        );
        let number = content_type_bits(ImeContentType::Number);
        assert_eq!(number.purpose, 3, "content_purpose = number");
        assert_eq!(number.hint, 0, "金额输入不隐藏、不敏感");
    }

    /// R10-5：Ctrl/Alt/Super 组合必须被拦下；Shift 与 AltGr（无对应位）必须放行。
    #[test]
    fn command_modifiers_block_character_text_but_shift_and_altgr_do_not() {
        assert!(text_blocked_by_modifiers(Modifiers::CONTROL));
        assert!(text_blocked_by_modifiers(Modifiers::ALT));
        assert!(text_blocked_by_modifiers(Modifiers::META));
        assert!(text_blocked_by_modifiers(
            Modifiers::CONTROL | Modifiers::SHIFT
        ));
        assert!(
            !text_blocked_by_modifiers(Modifiers::SHIFT),
            "Shift 是正常大写输入"
        );
        assert!(!text_blocked_by_modifiers(Modifiers::empty()));
        // AltGr 在 xkb 里是 Mod5，`Modifiers` 没有对应位 ⇒ 不过滤（欧洲布局靠它输入 @ € { }）。
        assert!(!text_blocked_by_modifiers(Modifiers::SHIFT));
    }
}

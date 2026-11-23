use std::time::{Duration, Instant};

use crate::{
    event::Modifiers,
    math::{LogicalRect, px},
    window::ImeContentType,
};

pub(crate) const NO_IM_FALLBACK: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FocusChange {
    pub disable: bool,
    pub enable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowImePolicy {
    app_enabled: bool,
    text_target: bool,
}

impl Default for WindowImePolicy {
    fn default() -> Self {
        Self {
            app_enabled: true,
            text_target: false,
        }
    }
}

impl WindowImePolicy {
    const fn allows_text_input(self) -> bool {
        self.app_enabled && self.text_target
    }
}

#[derive(Debug, Default)]
pub(crate) struct TextInputRoute {
    focus: Option<u32>,
    keyboard_focus: Option<u32>,
    policies: Vec<WindowImePolicy>,
    commits: u32,
    acked: u32,
    saw_im_event: bool,
    degraded: bool,
    first_enable_at: Option<Instant>,
}

impl TextInputRoute {
    pub(crate) fn add_window(&mut self) {
        self.policies.push(WindowImePolicy::default());
    }

    pub(crate) fn set_policy(&mut self, window: u32, app_enabled: bool, text_target: bool) {
        let Some(policy) = self.policies.get_mut(window as usize) else {
            debug_assert!(
                false,
                "窗口策略表与 WaylandState::windows 不同步（缺下标 {window}）"
            );
            return;
        };
        *policy = WindowImePolicy {
            app_enabled,
            text_target,
        };
    }

    pub(crate) fn focus_in(&mut self, window: u32) {
        self.keyboard_focus = Some(window);
    }

    pub(crate) fn focus_out(&mut self, window: u32) {
        if self.keyboard_focus == Some(window) {
            self.keyboard_focus = None;
        }
    }

    pub(crate) fn reconcile(&mut self, now: Instant) -> FocusChange {
        let desired = self.desired_focus();
        if desired == self.focus {
            return FocusChange::default();
        }
        let change = FocusChange {
            disable: self.focus.is_some(),
            enable: desired.is_some(),
        };
        self.focus = desired;
        if change.enable {
            self.first_enable_at.get_or_insert(now);
        }
        change
    }

    fn desired_focus(&self) -> Option<u32> {
        let window = self.keyboard_focus?;
        let policy = self.policies.get(window as usize)?;
        policy.allows_text_input().then_some(window)
    }

    pub(crate) fn disable(&mut self) -> FocusChange {
        self.keyboard_focus = None;
        if self.focus.take().is_some() {
            FocusChange {
                disable: true,
                enable: false,
            }
        } else {
            FocusChange::default()
        }
    }

    #[must_use]
    pub(crate) fn focus(&self) -> Option<u32> {
        self.focus
    }

    #[must_use]
    pub(crate) fn is_focus(&self, window: u32) -> bool {
        self.focus == Some(window)
    }

    #[must_use]
    pub(crate) fn is_keyboard_focus(&self, window: u32) -> bool {
        self.keyboard_focus == Some(window)
    }

    pub(crate) fn reindex_removed(&mut self, removed: u32) {
        let removed = removed as usize;
        if removed < self.policies.len() {
            self.policies.remove(removed);
        }
        self.focus = shifted(self.focus, removed);
        self.keyboard_focus = shifted(self.keyboard_focus, removed);
    }

    pub(crate) fn note_commit(&mut self) {
        self.commits = self.commits.saturating_add(1);
    }

    #[must_use]
    pub(crate) fn commit_in_flight(&self) -> bool {
        self.commits != self.acked
    }

    #[must_use]
    pub(crate) fn settle(&mut self, serial: u32) -> bool {
        if serial != self.commits {
            return false;
        }
        self.acked = serial;
        true
    }

    pub(crate) fn note_im_event(&mut self) {
        self.saw_im_event = true;
        self.degraded = false;
    }

    #[must_use]
    pub(crate) fn takes_text(&self) -> bool {
        self.focus.is_some() && !self.degraded
    }

    pub(crate) fn should_degrade(&mut self, now: Instant) -> bool {
        if self.degraded || self.saw_im_event {
            return false;
        }
        let Some(first_enable_at) = self.first_enable_at else {
            return false;
        };
        if now.duration_since(first_enable_at) < NO_IM_FALLBACK {
            return false;
        }
        self.degraded = true;
        true
    }

    #[must_use]
    pub(crate) fn next_degrade_in(&self, now: Instant) -> Option<Duration> {
        if self.degraded || self.saw_im_event {
            return None;
        }
        let first_enable_at = self.first_enable_at?;
        Some(NO_IM_FALLBACK.saturating_sub(now.duration_since(first_enable_at)))
    }
}

fn shifted(index: Option<u32>, removed: usize) -> Option<u32> {
    match index {
        Some(index) if index as usize == removed => None,
        Some(index) if index as usize > removed => Some(index - 1),
        other => other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ImeStateSnapshot {
    pub cursor_area: Option<LogicalRect>,
    pub content: ContentTypeBits,
}

impl ImeStateSnapshot {
    #[must_use]
    pub(crate) fn cursor_rect(self) -> (i32, i32, i32, i32) {
        let area = self
            .cursor_area
            .unwrap_or_else(|| LogicalRect::new(px(0.0), px(0.0), px(1.0), px(1.0)));
        (
            area.origin.x.value().round() as i32,
            area.origin.y.value().round() as i32,
            area.size.width.max(px(1.0)).value().round() as i32,
            area.size.height.max(px(1.0)).value().round() as i32,
        )
    }

    #[must_use]
    pub(crate) fn diff_from(self, applied: Option<Self>) -> Option<ImeStateDiff> {
        let applied_cursor = applied.map(|snapshot| snapshot.cursor_area);
        let applied_content = applied.map(|snapshot| snapshot.content);
        let cursor_rect = (applied_cursor != Some(self.cursor_area)).then(|| self.cursor_rect());
        let content = (applied_content != Some(self.content)).then_some(self.content);
        if cursor_rect.is_none() && content.is_none() {
            return None;
        }
        Some(ImeStateDiff {
            cursor_rect,
            content,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImeStateDiff {
    pub cursor_rect: Option<(i32, i32, i32, i32)>,
    pub content: Option<ContentTypeBits>,
}

pub(crate) const CONTENT_HINT_HIDDEN_TEXT: u32 = 0x40;
pub(crate) const CONTENT_HINT_SENSITIVE_DATA: u32 = 0x80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentTypeBits {
    pub hint: u32,
    pub purpose: u32,
}

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
        ImeContentType::Number => ContentTypeBits {
            hint: 0,
            purpose: 3,
        },
    }
}

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
    use crate::{
        event::Modifiers,
        math::{LogicalRect, px},
        window::ImeContentType,
    };

    fn route_with_text_targets(windows: &[u32]) -> TextInputRoute {
        let mut route = TextInputRoute::default();
        let count = windows.iter().max().map_or(0, |max| max + 1);
        for _ in 0..count {
            route.add_window();
        }
        for &window in windows {
            route.set_policy(window, true, true);
        }
        route
    }

    fn route_with_focus(now: Instant) -> TextInputRoute {
        let mut route = route_with_text_targets(&[0]);
        route.focus_in(0);
        let change = route.reconcile(now);
        assert!(change.enable && !change.disable);
        route.note_commit();
        route
    }

    #[test]
    fn focus_changes_disable_the_previous_surface_first() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[1, 2]);
        route.focus_in(1);
        assert_eq!(
            route.reconcile(now),
            super::FocusChange {
                disable: false,
                enable: true
            }
        );
        assert_eq!(route.focus(), Some(1));
        route.focus_in(2);
        assert_eq!(
            route.reconcile(now),
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
        let mut route = route_with_text_targets(&[3]);
        route.focus_in(3);
        assert!(route.reconcile(now).enable);
        route.focus_in(3);
        assert_eq!(route.reconcile(now), super::FocusChange::default());
    }

    #[test]
    fn only_the_focused_window_is_reported_as_the_ime_target() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[5]);
        route.focus_in(5);
        assert_eq!(
            route.reconcile(now),
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

    #[test]
    fn disable_is_idempotent_and_reports_whether_it_changed_anything() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[1]);
        assert_eq!(route.disable(), super::FocusChange::default());
        route.focus_in(1);
        assert!(route.reconcile(now).enable);
        assert_eq!(
            route.disable(),
            super::FocusChange {
                disable: true,
                enable: false
            }
        );
        assert_eq!(route.disable(), super::FocusChange::default());
    }

    #[test]
    fn removing_a_window_reindexes_the_ime_target() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[0, 1, 2, 3]);
        route.focus_in(3);
        assert!(route.reconcile(now).enable);
        route.reindex_removed(1);
        assert_eq!(route.focus(), Some(2));
        route.reindex_removed(2);
        assert_eq!(route.focus(), None);
        route.set_policy(1, true, true);
        route.focus_in(1);
        assert!(route.reconcile(now).enable);
        route.reindex_removed(5);
        assert_eq!(route.focus(), Some(1));
    }

    #[test]
    fn only_a_matching_done_serial_allows_new_state_to_be_sent() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        assert!(route.commit_in_flight(), "刚 commit 过 ⇒ 有批在飞");
        assert!(route.settle(1), "commit 次数 = 1 ⇒ serial 1 匹配");
        assert!(
            !route.commit_in_flight(),
            "匹配的 done 之后不得再有「在飞的批」，否则 done 会继续制造 commit"
        );
        route.note_commit();
        assert!(route.settle(2));
        assert!(
            !route.settle(1),
            "过期的 serial ⇒ 文本照常应用，但不得下发 set_content_type 等状态"
        );
        assert!(!route.settle(3), "未来的 serial 同样不匹配");
        assert!(!route.commit_in_flight(), "不匹配的 done 不得改动 ack 进度");
    }

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

    #[test]
    fn a_late_im_event_restores_ime_ownership() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        assert!(route.should_degrade(now + NO_IM_FALLBACK));
        assert!(!route.takes_text());
        route.note_im_event();
        assert!(route.takes_text(), "IM 一旦说话就必须重新独占文本通路");
    }

    #[test]
    fn switching_focus_does_not_restart_the_fallback_timer() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[7, 8]);
        route.focus_in(7);
        assert!(route.reconcile(now).enable);
        route.note_commit();
        let switched_at = now + Duration::from_millis(100);
        route.focus_in(8);
        let change = route.reconcile(switched_at);
        assert!(
            change.disable && change.enable,
            "换 surface = 先 disable 再 enable"
        );
        assert_eq!(
            route.next_degrade_in(switched_at),
            Some(NO_IM_FALLBACK - Duration::from_millis(100)),
            "换焦点不能把 1.5 s 的窗口期重新发一遍"
        );
        assert!(
            route.should_degrade(now + NO_IM_FALLBACK),
            "降级时刻仍以首次 enable 为起点"
        );
        assert!(!route.takes_text());
    }

    #[test]
    fn switching_focus_does_not_undo_a_degraded_seat() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[7, 8]);
        route.focus_in(7);
        assert!(route.reconcile(now).enable);
        assert!(route.should_degrade(now + NO_IM_FALLBACK));
        assert!(!route.takes_text());

        let later = now + NO_IM_FALLBACK * 2;
        route.focus_in(8);
        assert!(route.reconcile(later).enable);
        assert!(
            !route.takes_text(),
            "换焦点后键盘文本必须继续产生（旧实现会在这里再开一个 1.5 s 的真空）"
        );
        assert!(
            !route.should_degrade(later + NO_IM_FALLBACK),
            "降级状态是 per-seat 的，不会因为换 surface 重新判定一次"
        );
        assert_eq!(route.next_degrade_in(later), None, "已经判定过，不再计时");
    }

    #[test]
    fn disabling_the_focus_does_not_forget_the_seat_verdict() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[0, 2]);
        route.focus_in(0);
        assert!(route.reconcile(now).enable);
        assert!(route.should_degrade(now + NO_IM_FALLBACK));
        route.disable();
        assert!(!route.takes_text(), "没有焦点时键盘通路本来就该产生文本");

        route.focus_in(2);
        assert!(route.reconcile(now + NO_IM_FALLBACK * 3).enable);
        assert!(
            !route.takes_text(),
            "重新聚焦不得再开一个 1.5 s 的 IME 窗口期"
        );
        assert_eq!(route.next_degrade_in(now + NO_IM_FALLBACK * 3), None);
    }

    #[test]
    fn an_im_event_is_remembered_for_the_whole_seat() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[0, 3]);
        route.focus_in(0);
        assert!(route.reconcile(now).enable);
        route.note_im_event();
        route.disable();
        route.focus_in(3);
        assert!(route.reconcile(now + Duration::from_secs(10)).enable);
        assert!(route.takes_text());
        assert!(!route.should_degrade(now + Duration::from_secs(60)));
        assert_eq!(route.next_degrade_in(now + Duration::from_secs(60)), None);
    }

    #[test]
    fn a_non_text_focus_neither_enables_nor_disables() {
        let now = Instant::now();
        let mut route = TextInputRoute::default();
        route.add_window();
        route.set_policy(0, true, false);
        route.focus_in(0);
        assert_eq!(
            route.reconcile(now),
            super::FocusChange::default(),
            "没有文本目标 ⇒ 协议对象保持 disable（不得凭空 enable）"
        );
        assert_eq!(route.focus(), None);
        assert!(!route.takes_text(), "非编辑焦点上字符走键盘通路");
    }

    #[test]
    fn an_app_disabled_text_target_is_disabled_and_yields_text_to_the_keyboard() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        assert!(route.takes_text(), "前置：IME 已接管文本");

        route.set_policy(0, false, true);
        assert_eq!(
            route.reconcile(now),
            super::FocusChange {
                disable: true,
                enable: false
            },
            "应用禁用 ⇒ 协议 disable（旧实现只写 App 状态、从不产生 disable）"
        );
        assert_eq!(route.focus(), None);
        assert!(!route.takes_text(), "禁用后键盘通路必须产生字符");
    }

    #[test]
    fn re_enabling_a_text_target_enables_the_surface_again() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        route.set_policy(0, false, true);
        assert!(route.reconcile(now).disable);
        route.note_commit();

        route.set_policy(0, true, true);
        assert_eq!(
            route.reconcile(now),
            super::FocusChange {
                disable: false,
                enable: true
            },
            "策略恢复 + 仍聚焦文本 ⇒ 重新 enable"
        );
        assert_eq!(route.focus(), Some(0));
        assert!(route.takes_text());
    }

    #[test]
    fn a_focus_change_while_app_disabled_never_enables() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[0, 1]);
        route.set_policy(0, false, true);
        route.set_policy(1, false, true);

        route.focus_in(0);
        assert_eq!(route.reconcile(now), super::FocusChange::default());
        route.focus_in(1);
        assert_eq!(
            route.reconcile(now),
            super::FocusChange::default(),
            "禁用态换焦点不得 enable（旧实现在每个 enter 上无条件 enable）"
        );
        route.focus_out(1);
        assert_eq!(route.reconcile(now), super::FocusChange::default());
        assert!(!route.takes_text());
    }

    #[test]
    fn a_lost_text_target_emits_disable_exactly_once() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        route.set_policy(0, true, false);
        assert_eq!(
            route.reconcile(now),
            super::FocusChange {
                disable: true,
                enable: false
            }
        );
        assert_eq!(
            route.reconcile(now),
            super::FocusChange::default(),
            "已经 disable ⇒ 后续轮次不得重复 disable（⇒ End 不得重复）"
        );
    }

    #[test]
    fn text_to_non_text_to_text_yields_one_disable_and_one_enable() {
        let now = Instant::now();
        let mut route = route_with_focus(now);

        route.set_policy(0, true, false);
        let lost = route.reconcile(now);
        route.note_commit();
        route.set_policy(0, true, true);
        let regained = route.reconcile(now);
        route.note_commit();

        assert_eq!(
            lost,
            super::FocusChange {
                disable: true,
                enable: false
            }
        );
        assert_eq!(
            regained,
            super::FocusChange {
                disable: false,
                enable: true
            }
        );
        assert_eq!(route.focus(), Some(0));
        assert!(route.takes_text());
        assert!(route.settle(3));
        assert!(!route.commit_in_flight());
    }

    #[test]
    fn policy_changes_while_a_commit_is_in_flight_do_not_disturb_the_serial_state() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        assert!(route.commit_in_flight(), "前置：首次 enable 的 commit 在飞");

        route.set_policy(0, false, true);
        let change = route.reconcile(now);
        assert!(change.disable);
        assert!(
            route.commit_in_flight(),
            "策略变化本身不 commit（提交由平台层在收敛点统一发）"
        );
        route.note_commit();

        assert!(!route.settle(1), "在飞的旧 batch（serial 1）不得放行新状态");
        assert!(route.settle(2), "匹配 serial 2 之后才允许回写");
        assert!(!route.commit_in_flight());
        assert!(!route.reconcile(now).disable, "已经 disable，不重复变化");
    }

    #[test]
    fn a_stale_focus_out_does_not_clear_the_new_focus() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[0, 1]);
        route.focus_in(1);
        assert!(route.reconcile(now).enable);
        assert!(route.is_keyboard_focus(1), "候选目标 = 键盘焦点窗口");
        route.focus_out(0);
        assert_eq!(route.reconcile(now), super::FocusChange::default());
        assert_eq!(route.focus(), Some(1));
        assert!(route.is_keyboard_focus(1), "迟到的 leave 不得清掉新候选");
        route.focus_out(1);
        assert!(route.reconcile(now).disable);
        assert_eq!(route.focus(), None);
        assert!(!route.is_keyboard_focus(1), "失焦后不再持有候选");
    }

    #[test]
    fn ime_state_snapshots_only_differ_when_a_field_changes() {
        let area = super::ImeStateSnapshot {
            cursor_area: Some(LogicalRect::new(px(10.0), px(20.0), px(2.0), px(18.0))),
            content: content_type_bits(ImeContentType::Text),
        };
        assert_eq!(
            area, area,
            "同一批状态必须相等（否则每个 done 都会 commit）"
        );
        assert_ne!(
            area,
            super::ImeStateSnapshot {
                cursor_area: Some(LogicalRect::new(px(11.0), px(20.0), px(2.0), px(18.0))),
                ..area
            },
            "caret 移动必须被看见"
        );
        assert_ne!(
            area,
            super::ImeStateSnapshot {
                cursor_area: None,
                ..area
            },
            "应用撤销 caret hint 也必须回写"
        );
        assert_ne!(
            area,
            super::ImeStateSnapshot {
                content: content_type_bits(ImeContentType::Password),
                ..area
            },
            "内容语义变化（密码框）必须回写"
        );
    }

    #[test]
    fn a_missing_caret_hint_falls_back_to_a_one_pixel_rect() {
        let snapshot = super::ImeStateSnapshot {
            cursor_area: None,
            content: content_type_bits(ImeContentType::Text),
        };
        assert_eq!(snapshot.cursor_rect(), (0, 0, 1, 1));

        let hinted = super::ImeStateSnapshot {
            cursor_area: Some(LogicalRect::new(px(12.4), px(30.6), px(0.0), px(17.2))),
            ..snapshot
        };
        assert_eq!(
            hinted.cursor_rect(),
            (12, 31, 1, 17),
            "坐标四舍五入、宽高至少 1（0 宽是非法参数）"
        );
    }

    #[test]
    fn the_same_state_is_never_committed_twice() {
        let text = content_type_bits(ImeContentType::Text);
        let snapshot = super::ImeStateSnapshot {
            cursor_area: Some(LogicalRect::new(px(10.0), px(20.0), px(2.0), px(18.0))),
            content: text,
        };

        let first = snapshot.diff_from(None).expect("enable 之后必须整份重发");
        assert_eq!(first.cursor_rect, Some((10, 20, 2, 18)));
        assert_eq!(first.content, Some(text));

        assert_eq!(
            snapshot.diff_from(Some(snapshot)),
            None,
            "连续相同状态不得产生第二次 commit"
        );
        assert_eq!(snapshot.diff_from(Some(snapshot)), None);

        let moved = super::ImeStateSnapshot {
            cursor_area: Some(LogicalRect::new(px(10.0), px(40.0), px(2.0), px(18.0))),
            ..snapshot
        };
        assert_eq!(
            moved.diff_from(Some(snapshot)),
            Some(super::ImeStateDiff {
                cursor_rect: Some((10, 40, 2, 18)),
                content: None,
            }),
            "caret 移动只重发 caret 矩形"
        );
        let password = super::ImeStateSnapshot {
            content: content_type_bits(ImeContentType::Password),
            ..snapshot
        };
        assert_eq!(
            password.diff_from(Some(snapshot)),
            Some(super::ImeStateDiff {
                cursor_rect: None,
                content: Some(content_type_bits(ImeContentType::Password)),
            }),
            "密码框只重发 content_type（安全语义）"
        );
    }

    #[test]
    fn a_committed_empty_caret_hint_is_not_resent() {
        let no_hint = super::ImeStateSnapshot {
            cursor_area: None,
            content: content_type_bits(ImeContentType::Text),
        };
        let first = no_hint.diff_from(None).expect("第一次必须发");
        assert_eq!(first.cursor_rect, Some((0, 0, 1, 1)));
        assert_eq!(
            no_hint.diff_from(Some(no_hint)),
            None,
            "已经提交过「无 hint」⇒ 退化的 1×1 不该每个 done 都重发"
        );
    }

    #[test]
    fn no_focus_means_the_keyboard_path_always_owns_text() {
        let route = TextInputRoute::default();
        assert!(!route.takes_text());
        assert_eq!(route.focus(), None);
        assert_eq!(route.next_degrade_in(Instant::now()), None);
    }

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
        assert!(!text_blocked_by_modifiers(Modifiers::SHIFT));
    }
}

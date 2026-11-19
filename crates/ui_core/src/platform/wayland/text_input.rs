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
//! 降级        =  本进程首次 enable 之后 NO_IM_FALLBACK 内**没有任何** IM 事件（preedit/commit/delete）
//! 升级        =  降级后收到任何 IM 事件 ⇒ 立刻交回 IME
//! ```
//!
//! 「无任何 IM 事件」是唯一可观测的存活信号，所以降级必须是**启发式 + 可逆**的。
//!
//! # 降级计时属于 **seat**，不属于 surface（AR3 §3.2）
//!
//! 输入法是 **per-seat** 的：`zwp_text_input_v3` 只有一个对象，`enter`/`leave` 只是把它的
//! *焦点* 在 surface 之间搬动，**它背后的输入法引擎没有变**。因此「本 seat 没有输入法」这个
//! 判定不能跟着焦点走：旧实现每次 `enter` 都把 `degraded` / `saw_im_event` / 计时起点复位，
//! 于是在没有输入法守护进程的 compositor（sway / Hyprland 默认只暴露 text-input-v3、不跑
//! 输入法）上，用户**每切一次文本焦点就再吃一次 1.5 s 的键盘文本真空**。
//!
//! 现在三样都按 seat 记账、跨焦点切换保持：
//!
//! - `first_enable_at`：本进程**第一次** `enable` 的时刻（降级计时的唯一起点，只设一次）；
//! - `degraded`：一旦判定就保持，只有 `note_im_event`（输入法真的说话了）才撤销；
//! - `saw_im_event`：一旦见过 IM 事件就永久为真（这个 seat 有输入法）。
//!
//! 慢启动的输入法仍然能升回来（[`TextInputRoute::note_im_event`]），因为组合期的
//! `preedit_string` 会立刻把文本通路交回 IME。
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
//! [`TextInputRoute::settle`] 的返回值决定 —— **并且只有真的攒下了新状态才允许 `commit()`**：
//! 协议要求 compositor 对每个 `commit()` 回一条 `done`，若 `done` 里无条件再 `commit()`，
//! 就是 done → commit → done 的永久自激（AR3 §3.3）。因此「要不要回写」由调用方用
//! [`ImeStateSnapshot`] 与上次已提交快照做差来决定，本模块只提供两个计数：
//! `commits`（已发出的 `commit()` 次数 = 下一个 `done` 的期望 serial）与 `acked`
//! （最后一次匹配上的 serial）；`commits != acked` ⇒ 有批在飞，新的状态更新应等 `done`。
//!
//! # B9：焦点是否「可编辑」决定 enable / disable
//!
//! `zwp_text_input_v3` 只有「焦点 surface」一个概念，**没有**「当前焦点是不是输入框」的协议
//! 请求。XML `enable`：*"This request must be issued every time the focused text input changes
//! to a new one, including within the current surface. Use zwp_text_input_v3.disable when there
//! is no longer any input focus on the current surface."* —— 焦点从输入框移到**同一个 surface
//! 内**的图表/列表（或应用 `set_ime_enabled(false)`）时也必须 `disable()`，否则 compositor
//! 侧永远认为该 surface 有文本焦点：候选窗不消失、按键被输入法路由走却没有任何文本目标。
//!
//! 本状态机因此把「协议真实状态」和「候选目标」分开：
//!
//! ```text
//! 每窗口策略 = (应用开关 app_enabled, 文本目标 text_target)   ← 泵每轮 dispatch 协议事件前同步
//! 候选目标   = 键盘焦点窗口（wl_keyboard.enter / text_input.enter）
//! desired    = 候选目标.filter(策略允许 enable)              ← 唯一的收敛判据点
//! 协议动作    = desired 与已 enable 的 focus 不同 ⇒ disable / enable（disable 优先）
//! ```
//!
//! 收敛只在 [`TextInputRoute::reconcile`] 里发生一次 ⇒ 不重复 `commit()`、不重复补
//! `ImeEvent::End`；策略不允许时的 `focus()` 恒 `None`，[`Self::takes_text`] 因此为 `false`，
//! 按键走普通键盘通路（`InputEvent::Text`）。

use std::time::{Duration, Instant};

use crate::{event::Modifiers, math::Rect, window::ImeContentType};

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

/// 一个窗口的 IME 策略（B9）：应用开关 × 文本目标。
///
/// 两个信号都由泵每轮从 `App` 同步（`sync_ime_policy`，在 dispatch 协议事件**之前**）：
///
/// - `app_enabled` = `App::ime_enabled`（`App::set_ime_enabled`，默认 `true`）；
/// - `text_target` = `App::ime_cursor_area` 是否 `Ok(Some(_))` —— 应用声明的 caret hint，
///   即「焦点落在可编辑文本上」。应用在焦点离开文本节点时必须清 hint
///   （`App::clear_ime_cursor_area`），D6(a) 的裁决口径。
///
/// 只有两者同时为真才允许 `enable`；`text_target == false`（焦点在图表/列表/无焦点）或应用
/// 禁用时协议状态必须是 `disable` —— 这是「同一 surface 内焦点从文本移到非文本也必须
/// disable」的唯一判据点。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowImePolicy {
    /// 应用开关（`App::ime_enabled`）。
    app_enabled: bool,
    /// 应用声明的文本目标（`App::ime_cursor_area` 有值）。
    text_target: bool,
}

impl Default for WindowImePolicy {
    fn default() -> Self {
        // `app_enabled` 默认与 `App::ime_enabled` 一致（启用）；`text_target` 默认 `false` ——
        // 应用声明 caret hint 之前没有可编辑焦点，不得凭「窗口存在」就 enable。
        Self {
            app_enabled: true,
            text_target: false,
        }
    }
}

impl WindowImePolicy {
    /// 是否允许 text-input 在该窗口上 enable。
    const fn allows_text_input(self) -> bool {
        self.app_enabled && self.text_target
    }
}

/// `zwp_text_input_v3` 的客户端侧状态机（纯逻辑，可在开发机上单测）。
///
/// 协议对象的真实状态是 `focus`（已 enable 的窗口），候选目标是 `keyboard_focus`；二者之间
/// 的差由 [`Self::reconcile`] 收敛，是否允许 enable 由每窗口策略（[`WindowImePolicy`]）决定。
#[derive(Debug, Default)]
pub(crate) struct TextInputRoute {
    /// 当前已 `enable` 的窗口下标（协议真实状态；`None` = text-input 对象处于 disable）。
    focus: Option<u32>,
    /// 键盘焦点窗口下标（候选目标；策略不允许时存在但不 enable）。
    ///
    /// `wl_keyboard.enter` / `zwp_text_input_v3.enter` 更新（两个信号是同一焦点变化的两个来源，
    /// 收敛到同一个候选）。
    keyboard_focus: Option<u32>,
    /// 每窗口 IME 策略（下标与 `WaylandState::windows` 一一对应）。
    ///
    /// [`Self::add_window`] / [`Self::reindex_removed`] 维护增删与下标前移；策略值由泵每轮从
    /// `App` 覆盖写入（[`Self::set_policy`]）。
    policies: Vec<WindowImePolicy>,
    /// 已发出的 `commit()` 次数（`done.serial` 的期望值）。
    commits: u32,
    /// 最后一次**匹配上的** `done.serial`（= 最近一次被 compositor 确认的 `commit()`）。
    ///
    /// `commits == acked` ⇒ 没有批在飞；否则新的状态更新应等 `done`（协议：*"All pending state
    /// requests … should be sent and committed after receiving a done event with a matching
    /// serial."*）。
    acked: u32,
    /// 本 seat 是否观察到过 IM 事件（preedit / commit / delete_surrounding）。
    ///
    /// **跨焦点保持**：输入法属于 seat，不属于 surface（AR3 §3.2）。
    saw_im_event: bool,
    /// 无 IM 守护进程的降级（键盘文本通路接管）。跨焦点保持，只有 [`Self::note_im_event`] 撤销。
    degraded: bool,
    /// 本进程**第一次** `enable()` 的时刻（降级计时的唯一起点）。
    ///
    /// 只设一次、从不因焦点切换复位：否则「每换一次焦点重新给 1.5 s 窗口期」会让无输入法的
    /// compositor 上每次换焦点都有 1.5 s 文本真空（AR3 §3.2）。策略不允许的焦点不会 enable，
    /// 因此也不会提前开始计时。
    first_enable_at: Option<Instant>,
}

impl TextInputRoute {
    /// 新增一个窗口（下标 = 当前窗口数）；默认策略「应用启用、无文本目标」。
    pub(crate) fn add_window(&mut self) {
        self.policies.push(WindowImePolicy::default());
    }

    /// 写入一个窗口的策略（`sync_ime_policy` 每轮调用；**不**在这里收敛协议状态）。
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

    /// 键盘焦点进入 `window`（`wl_keyboard.enter` / `zwp_text_input_v3.enter`）。
    ///
    /// **不**立即 enable：是否真的接管文本要等 [`Self::reconcile`] 按策略判定（焦点在非编辑
    /// 控件上时保持 disable，B9）。
    pub(crate) fn focus_in(&mut self, window: u32) {
        self.keyboard_focus = Some(window);
    }

    /// 键盘焦点离开 `window`（`wl_keyboard.leave` / `zwp_text_input_v3.leave`）。
    ///
    /// 只清「正是这个窗口」的候选：迟到的 leave 不得清掉新焦点的候选。
    pub(crate) fn focus_out(&mut self, window: u32) {
        if self.keyboard_focus == Some(window) {
            self.keyboard_focus = None;
        }
    }

    /// 按「键盘焦点 × 每窗口策略」收敛协议状态，返回需要下发的 enable/disable。
    ///
    /// **唯一的收敛点**（B9）：`desired = 键盘焦点窗口.filter(策略允许)`，与已 enable 的
    /// `focus` 不同才产生一次变化。因此：
    ///
    /// - 焦点在非编辑元素上（`text_target == false`）⇒ `desired == None` ⇒ `disable`；
    /// - 应用 `ImmAssociate` 等价物 `set_ime_enabled(false)` ⇒ 同上 disable，且
    ///   [`Self::takes_text`] 恒 `false`（按键走普通键盘通路）；
    /// - 重新允许且仍聚焦文本 ⇒ enable（调用方整份重发 `set_content_type` /
    ///   `set_cursor_rectangle`）；
    /// - 策略/焦点没变 ⇒ 空变化：不重复 `commit()`，也不会重复补 `ImeEvent::End`。
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
            // 降级计时起点：本 seat **第一次真正 enable** 的时刻（只设一次，跨焦点切换保持）。
            self.first_enable_at.get_or_insert(now);
        }
        change
    }

    /// 策略允许的启用目标（= 键盘焦点窗口，若其策略允许 text-input）。
    fn desired_focus(&self) -> Option<u32> {
        let window = self.keyboard_focus?;
        let policy = self.policies.get(window as usize)?;
        policy.allows_text_input().then_some(window)
    }

    /// 关闭窗口 / 退出：无条件停用（清掉候选与协议焦点）。
    ///
    /// 降级状态**不清**：本 seat 有没有输入法不因「当前没有文本焦点」而改变（下一次 enable
    /// 若重新计时，就又把 1.5 s 真空还给了没有输入法的 compositor）。
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

    /// 该窗口是否持有键盘焦点（候选目标，可能因策略不允许而尚未 enable）。
    #[must_use]
    pub(crate) fn is_keyboard_focus(&self, window: u32) -> bool {
        self.keyboard_focus == Some(window)
    }

    /// 一个窗口被移除后修正下标（`windows` 是 `Vec`，移除位置之后的下标整体 -1）。
    ///
    /// 被移除的正是焦点 / 键盘焦点时置 `None`（调用方在此之前已经 `disable` 过协议对象）；
    /// 每窗口策略表同步移除。其它窗口的下标按同一套规则前移。
    pub(crate) fn reindex_removed(&mut self, removed: u32) {
        let removed = removed as usize;
        if removed < self.policies.len() {
            self.policies.remove(removed);
        }
        self.focus = shifted(self.focus, removed);
        self.keyboard_focus = shifted(self.keyboard_focus, removed);
    }

    /// 记录一次 `commit()` 请求（`done.serial` 的期望值 = 已发出的 commit 次数）。
    pub(crate) fn note_commit(&mut self) {
        self.commits = self.commits.saturating_add(1);
    }

    /// 是否有已发出、还没被 `done` 确认的 `commit()`。
    ///
    /// `true` ⇒ 新的状态更新应等这一轮 `done`（否则会与 compositor 的双缓冲状态错位，也会
    /// 让「done 之后再 commit」的判据失去意义）。
    #[must_use]
    pub(crate) fn commit_in_flight(&self) -> bool {
        self.commits != self.acked
    }

    /// `done.serial` 是否与本对象已发出的 `commit()` 次数一致（匹配 ⇒ 记下 ack）。
    ///
    /// `true` ⇒ 可以下发上一轮攒下的状态（`set_content_type` / `set_cursor_rectangle` 等）。
    /// `false` ⇒ 按协议**仍然应用文本变化**，但不要下发新状态（会与 compositor 的已知状态错位）。
    #[must_use]
    pub(crate) fn settle(&mut self, serial: u32) -> bool {
        if serial != self.commits {
            return false;
        }
        self.acked = serial;
        true
    }

    /// 收到任何 IM 事件：证明本 seat 有输入法引擎 ⇒ 撤销降级（永久，直到进程结束）。
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
        let Some(first_enable_at) = self.first_enable_at else {
            return false;
        };
        if now.duration_since(first_enable_at) < NO_IM_FALLBACK {
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
        let first_enable_at = self.first_enable_at?;
        Some(NO_IM_FALLBACK.saturating_sub(now.duration_since(first_enable_at)))
    }
}

/// 窗口下标重映射：移除 `removed` 之后，等于它的置 `None`、大于它的减一。
fn shifted(index: Option<u32>, removed: usize) -> Option<u32> {
    match index {
        Some(index) if index as usize == removed => None,
        Some(index) if index as usize > removed => Some(index - 1),
        other => other,
    }
}

// ── 已提交状态的快照（`done` 回写的「脏」判据，AR3 §3.3）──────────────────────

/// 一次 `commit()` 会下发的 text-input 状态（`set_surrounding_text` 本平台没有通路）。
///
/// 用途只有一件：和**上次已提交的快照**做差。`done` 之后只有快照真的变了才允许再
/// `commit()`；否则每个 `done` 都会制造下一次 `commit()`，而协议保证 compositor 对每个
/// `commit()` 必回 `done` ⇒ 事件循环被 done→commit→done 永久钉住（AR3 §3.3 / P1）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ImeStateSnapshot {
    /// 应用声明的 caret 区域（surface 逻辑坐标；`None` = 应用没有 hint）。
    pub cursor_area: Option<Rect>,
    /// `set_content_type` 的 `(content_hint, content_purpose)`。
    pub content: ContentTypeBits,
}

impl ImeStateSnapshot {
    /// `set_cursor_rectangle` 的四个参数（`x, y, width, height`）。
    ///
    /// 协议要求宽高 ≥ 1；应用没有 hint 时退化到原点 1×1（候选窗不会跑到屏幕角落之外）。
    #[must_use]
    pub(crate) fn cursor_rect(self) -> (i32, i32, i32, i32) {
        let area = self
            .cursor_area
            .unwrap_or_else(|| Rect::new(0.0, 0.0, 1.0, 1.0));
        (
            area.origin.x.round() as i32,
            area.origin.y.round() as i32,
            area.size.width.max(1.0).round() as i32,
            area.size.height.max(1.0).round() as i32,
        )
    }

    /// 相对**上次已提交快照**的差异：`None` ⇒ 完全一致 ⇒ 调用方**不得** `commit()`。
    ///
    /// 这就是 AR3 §3.3 的自激闸门，落在纯逻辑层：`done` 处理里拿当前状态与 `applied` 做差，
    /// 空 diff 就什么都不发 ⇒ 不会再制造下一个 `done` ⇒ done → commit → done 的环断在这里。
    ///
    /// `applied == None`（从没提交过，例如 `enable()` 之后）⇒ 整份重发；注意
    /// `applied: Some { cursor_area: None }` 与 `applied: None` **不是**一回事：前者表示
    /// "提交过『没有 caret hint』"，后者表示"一次都没提交过"。
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

/// 一次 `commit()` 要下发的差异（只发变了的那几项；`None` 的项不发请求）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImeStateDiff {
    /// 要 `set_cursor_rectangle` 的参数（`None` = caret 没变）。
    pub cursor_rect: Option<(i32, i32, i32, i32)>,
    /// 要 `set_content_type` 的位（`None` = 内容语义没变）。
    pub content: Option<ContentTypeBits>,
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
    use crate::{event::Modifiers, math::Rect, window::ImeContentType};

    /// 建一个「窗口数足够、指定窗口都是文本目标（应用启用）」的路由，供 R10 系列用例使用。
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

    /// 一个已 enable 的路由（窗口 0 是文本目标、应用启用、已 commit 一次）。
    fn route_with_focus(now: Instant) -> TextInputRoute {
        let mut route = route_with_text_targets(&[0]);
        route.focus_in(0);
        let change = route.reconcile(now);
        assert!(change.enable && !change.disable);
        route.note_commit();
        route
    }

    /// R10-6：焦点切换必须先 disable 旧的再 enable 新的（XML `enable` 的硬要求）。
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

    /// R10-6 的核心回归：`is_focus` 是「该不该为这个窗口停用 IME」的唯一判据 —— 关闭/失焦
    /// **非焦点**窗口时平台层不得动焦点窗口的 IME。
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

    /// `remove_window` 只在被移除窗口就是焦点时才停用。
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

    /// R10-6：窗口被移除后焦点下标必须跟着前移（否则 IME 会投给另一个窗口）；
    /// 每窗口策略表同步前移。
    #[test]
    fn removing_a_window_reindexes_the_ime_target() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[0, 1, 2, 3]);
        route.focus_in(3);
        assert!(route.reconcile(now).enable);
        // 移除下标 1（在焦点之前）⇒ 焦点 3 → 2，策略表同步前移。
        route.reindex_removed(1);
        assert_eq!(route.focus(), Some(2));
        // 移除焦点窗口 ⇒ 没有焦点（调用方负责在此之前 disable 协议对象）。
        route.reindex_removed(2);
        assert_eq!(route.focus(), None);
        // 移除之后的窗口不影响焦点。
        route.set_policy(1, true, true);
        route.focus_in(1);
        assert!(route.reconcile(now).enable);
        route.reindex_removed(5);
        assert_eq!(route.focus(), Some(1));
    }

    /// R10-13：`done` 的文本恒应用，但只有 serial 对上才允许下发新状态；匹配之后
    /// 「有没有批在飞」必须归零，否则 `done` 里就会再 `commit()` 一次（AR3 §3.3 的自激）。
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

    /// AR3 §3.2 P1：换焦点**不得**重置降级计时。
    ///
    /// 计时的起点是「本进程第一次 enable」，之后 `focus_in`/`focus_out`/`disable` 都不动它：否则
    /// 没有输入法的 compositor 上每换一次焦点都要重新等 1.5 s，而输入法根本不可能出现。
    #[test]
    fn switching_focus_does_not_restart_the_fallback_timer() {
        let now = Instant::now();
        let mut route = route_with_text_targets(&[7, 8]);
        route.focus_in(7);
        assert!(route.reconcile(now).enable);
        route.note_commit();
        // 在窗口 7 上启用后立刻换到窗口 8（例如 Tab 到下一个输入框）。
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

    /// AR3 §3.2 P1：已经降级之后换焦点不得把文本真空再发一次。
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

    /// 停用（窗口关闭 / 全部失焦）也不得清掉 seat 判定：下一次 enable 若重新计时，
    /// 没有输入法的 compositor 上每次重新聚焦都再吃 1.5 s。
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

    /// 输入法是 per-seat 的：任何一次 IM 事件都永久证明这个 seat 有引擎，
    /// 即使随后换了焦点、降级过也不会再回到键盘接管。
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

    // ── B9：文本目标 / 应用开关决定 enable / disable ──────────────────────────

    /// B9：键盘焦点落在**非编辑控件**上（同 surface 内的图表/列表）⇒ 不 enable、也不 disable。
    #[test]
    fn a_non_text_focus_neither_enables_nor_disables() {
        let now = Instant::now();
        let mut route = TextInputRoute::default();
        route.add_window();
        route.set_policy(0, true, false); // 应用启用，但焦点不是文本目标
        route.focus_in(0);
        assert_eq!(
            route.reconcile(now),
            super::FocusChange::default(),
            "没有文本目标 ⇒ 协议对象保持 disable（不得凭空 enable）"
        );
        assert_eq!(route.focus(), None);
        assert!(!route.takes_text(), "非编辑焦点上字符走键盘通路");
    }

    /// B9：应用 `set_ime_enabled(false)` ⇒ 已 enable 的 surface 必须 disable，
    /// 且 `takes_text() == false`（字母/数字作为普通 `InputEvent::Text` 到达应用）。
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

    /// B9：重新启用 + 文本焦点仍在 ⇒ 重新 enable（调用方据此整份重发 set_* 状态）。
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

    /// B9：应用处于禁用态时，在同窗口内换焦点不得 enable 任何 surface。
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

    /// B9 的「disable 是唯一收敛点」：文本目标消失只产生**一次** disable。
    ///
    /// 平台层在 disable 收敛时补一条 `ImeEvent::End`；收敛点唯一 ⇒ `End` 至多一条，
    /// 重复的策略同步（每轮泵都会跑）不会重复补。
    #[test]
    fn a_lost_text_target_emits_disable_exactly_once() {
        let now = Instant::now();
        let mut route = route_with_focus(now);
        route.set_policy(0, true, false); // 同一 surface 内焦点移到非编辑元素
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

    /// B9：同一 surface 内「文本目标 → 非文本 → 文本」恰好产生 disable → enable 各一次。
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
        // 两次变化各一次 commit ⇒ 下一次期望的 done serial 是 3（首次 enable 1 + disable 2
        // + enable 3）。serial 状态机不因策略变化而错位。
        assert!(route.settle(3));
        assert!(!route.commit_in_flight());
    }

    /// B9：`commit_in_flight` 期间策略变化只攒状态，不改变 commit/serial 记账 ——
    /// 平台层据此把 caret 更新推迟到匹配 serial 的 `done`（AR3 §3.3）。
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
        route.note_commit(); // 平台层为 disable 合成的 commit

        assert!(!route.settle(1), "在飞的旧 batch（serial 1）不得放行新状态");
        assert!(route.settle(2), "匹配 serial 2 之后才允许回写");
        assert!(!route.commit_in_flight());
        assert!(!route.reconcile(now).disable, "已经 disable，不重复变化");
    }

    /// B9：迟到的 `focus_out`（非当前候选）不得清掉新焦点的候选。
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

    /// AR3 §3.3：`ImeStateSnapshot` 是「要不要在 `done` 里回写」的唯一判据 ——
    /// 快照相同必须相等（调用方据此跳过 `commit()`），任何一项变化都要被看见。
    #[test]
    fn ime_state_snapshots_only_differ_when_a_field_changes() {
        let area = super::ImeStateSnapshot {
            cursor_area: Some(Rect::new(10.0, 20.0, 2.0, 18.0)),
            content: content_type_bits(ImeContentType::Text),
        };
        assert_eq!(
            area, area,
            "同一批状态必须相等（否则每个 done 都会 commit）"
        );
        assert_ne!(
            area,
            super::ImeStateSnapshot {
                cursor_area: Some(Rect::new(11.0, 20.0, 2.0, 18.0)),
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

    /// 没有 caret hint 时 `set_cursor_rectangle` 退化到原点 1×1（协议要求宽高 ≥ 1）。
    #[test]
    fn a_missing_caret_hint_falls_back_to_a_one_pixel_rect() {
        let snapshot = super::ImeStateSnapshot {
            cursor_area: None,
            content: content_type_bits(ImeContentType::Text),
        };
        assert_eq!(snapshot.cursor_rect(), (0, 0, 1, 1));

        let hinted = super::ImeStateSnapshot {
            cursor_area: Some(Rect::new(12.4, 30.6, 0.0, 17.2)),
            ..snapshot
        };
        assert_eq!(
            hinted.cursor_rect(),
            (12, 31, 1, 17),
            "坐标四舍五入、宽高至少 1（0 宽是非法参数）"
        );
    }

    /// AR3 §3.3 P1 的核心回归：**连续喂入相同的状态不得产生第二次 `commit()`**。
    ///
    /// 判据就是 `ImeStateSnapshot::diff_from`：
    ///
    /// - `applied == None`（`enable()` 之后）⇒ 整份重发（一次）；
    /// - 之后每个 `done` 回来时状态没变 ⇒ `None` ⇒ `surface.rs` 的 `done` 处理**什么都不发**，
    ///   因此不会再有下一次 `done`（协议：compositor 对每个 `commit()` 必回 `done`）；
    /// - 只有真的变了（caret 移动 / 内容语义变化）才有非空 diff —— 这时**才**允许 `commit()`。
    #[test]
    fn the_same_state_is_never_committed_twice() {
        let text = content_type_bits(ImeContentType::Text);
        let snapshot = super::ImeStateSnapshot {
            cursor_area: Some(Rect::new(10.0, 20.0, 2.0, 18.0)),
            content: text,
        };

        // 第一次（enable 之后）：整份重发。
        let first = snapshot.diff_from(None).expect("enable 之后必须整份重发");
        assert_eq!(first.cursor_rect, Some((10, 20, 2, 18)));
        assert_eq!(first.content, Some(text));

        // 第二个 done：状态没变 ⇒ 空 diff ⇒ 不得 commit（自激环在这里断掉）。
        assert_eq!(
            snapshot.diff_from(Some(snapshot)),
            None,
            "连续相同状态不得产生第二次 commit"
        );
        // 再喂一次也一样（不会因为「喂了几次」而变化）。
        assert_eq!(snapshot.diff_from(Some(snapshot)), None);

        // 真的变了才有 diff，而且只带变了的那一项。
        let moved = super::ImeStateSnapshot {
            cursor_area: Some(Rect::new(10.0, 40.0, 2.0, 18.0)),
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

    /// 「从没提交过」与「提交过『没有 caret hint』」必须区分：
    /// 后者不得因为 `Option` 的嵌套被当成没提交过而每次 `done` 都重发。
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

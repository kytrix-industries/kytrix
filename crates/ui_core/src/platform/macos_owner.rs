//! macOS 窗口层级的**平台无关纯逻辑**（X1 原生部分）：`WindowOptions.owner` /
//! `modal` / `always_on_top` → AppKit `NSWindowLevel` 的选择、owner 关系的解析，以及模态窗口
//! 打开/关闭时对 owner 施加/恢复的**原生 chrome 拦截**（RB-07）。
//!
//! # 为什么在 `platform/` 根而不是 `platform/macos/` 下
//!
//! `platform::macos` 整模块只在 macOS 目标编译（objc2 依赖也是 macOS-only）；这里的映射
//! 是纯数据 + 纯函数，必须能在开发机（Windows）单测 —— 与 [`crate::platform::macos_placement`]、
//! [`crate::platform::macos_close`] 同一模式。真机侧只把枚举翻成 AppKit 常量、调
//! `NSWindow.setLevel:` 与 `addChildWindow:ordered:`，以及按 [`OwnerChrome`] 读写
//! `ignoresMouseEvents` / 标准按钮的启用状态。
//!
//! # 层级选择
//!
//! | 条件 | 层级 | AppKit |
//! |---|---|---|
//! | 模态窗口（有 owner，`modal && owner.is_some()`） | [`WindowLevel::ModalPanel`] | `NSModalPanelWindowLevel`（8） |
//! | 置顶（`always_on_top`） | [`WindowLevel::Floating`] | `NSFloatingWindowLevel`（3） |
//! | 其它 | [`WindowLevel::Normal`] | `NSNormalWindowLevel`（0，不调用 `setLevel:`） |
//!
//! 优先级：**模态 > 置顶**。模态窗口是 owner 的临时交互面板，必须压过普通置顶窗口；
//! 两者同时为真时取更高的 `NSModalPanelWindowLevel`（8 > 3）。
//!
//! **模态不在原生层用 `runModalForWindow:`**：那会创建嵌套 run loop，与引擎自己的事件泵
//! 重入冲突（泵在 `NSApplication.run` 的 run loop 上跑，嵌套模态循环会让同一批回调重入）。
//! 引擎层的模态输入拦截 + 这里的层级 + `addChildWindow:` 三者合起来表达模态语义
//! （与 `WindowOptions::modal` 文档一致）。
//!
//! # 模态期对 owner 的原生拦截（RB-07，用户批准方案 A）
//!
//! 引擎层过滤输入只解决「事件被丢弃」，解决不了两件事：① owner 的原生 chrome（红点 /
//! 标题栏）仍可点击，与 Windows `EnableWindow(owner, FALSE)` 的语义不一致；② 用户点一下
//! owner 让它成为 key window 后，键盘事件进 owner 被模态过滤器丢弃 ⇒ **模态窗口收不到键**
//! （最严重的一条）。
//!
//! 因此模态窗口打开期间对 owner 的 `NSWindow` 施加 [`OwnerChrome::BLOCKED`]：窗口对鼠标
//! 透明（`setIgnoresMouseEvents:YES`）、不可由背景拖动、三个标准按钮禁用。打开前把 owner
//! 的**原值**快照下来，关闭时按 [`release_modal_owner`] 的决策逐项恢复（不假设默认值）。
//! 模态窗口显示时 `makeKeyAndOrderFront:` 接管键盘焦点，泵每轮再用 [`modal_takes_key`]
//! 把被 AppKit 窗口循环抢回 owner 的 key 拿回来；关闭时把 key 还给仍可用的 owner。
//!
//! **不用** `beginSheet:`（会改变窗口形态、把模态窗口嵌进 owner 的标题栏）与
//! `runModalForWindow:`（嵌套 run loop 与引擎泵冲突）。

use crate::window::WindowId;

/// 引擎窗口层级意图（映射到 AppKit `NSWindowLevel`；真机侧负责取常量值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowLevel {
    /// 普通窗口：保持 AppKit 默认层级，不调用 `setLevel:`。
    Normal,
    /// 置顶窗口：`NSFloatingWindowLevel`（浮在普通窗口之上，但低于系统面板）。
    Floating,
    /// owner 的模态窗口：`NSModalPanelWindowLevel`（高于置顶，压过普通窗口）。
    ModalPanel,
}

/// 选择窗口层级。`modal` 必须是**已解析出 owner** 的模态语义（调用方传
/// `options.modal && owner.is_some()`）：没有 owner 的模态没有意义（引擎层会降级为普通
/// 窗口），不应抬高层级。
pub(crate) const fn window_level(always_on_top: bool, modal: bool) -> WindowLevel {
    if modal {
        WindowLevel::ModalPanel
    } else if always_on_top {
        WindowLevel::Floating
    } else {
        WindowLevel::Normal
    }
}

/// 一个原生窗口的 owner 关系解析结果（X1）。
///
/// 引擎层保证 `WindowOptions.owner` 合法；平台侧仍可能因为窗口创建顺序找不到对应原生窗口
/// （`App` 的窗口表是 `HashMap`，创建批次顺序不可控）；那种情况降级为无 owner 并让调用方
/// 写诊断 —— 不 panic，窗口仍然可用，只是失去 transient parent 语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerResolution {
    /// 请求了 owner，且平台侧存在其原生窗口：挂接 `addChildWindow:ordered:`。
    Attach(WindowId),
    /// 未请求 owner：独立顶层窗口。
    None,
    /// 请求了 owner 但平台侧找不到（或自指）：降级为无 owner，调用方写诊断。
    Missing,
}

/// 把「请求的 owner」与「平台侧已存在的原生窗口 id 集合」对账成建窗后要挂接的关系。
///
/// 自指（`owner == Some(self)`）按非法处理：窗口不能是自己的 child（AppKit 会拒绝，行为
/// 未定义）；引擎层在建窗时也把这种 owner 视为不存在。
pub(crate) fn resolve_owner(
    id: WindowId,
    requested: Option<WindowId>,
    has_native: impl Fn(WindowId) -> bool,
) -> OwnerResolution {
    match requested {
        Some(owner) if owner != id && has_native(owner) => OwnerResolution::Attach(owner),
        Some(_) => OwnerResolution::Missing,
        None => OwnerResolution::None,
    }
}

/// owner 窗口 chrome 中**模态期会被改写**的那几项（快照与恢复的单位）。
///
/// 只有这几项受模态原生拦截影响：窗口对鼠标透明（`NSWindow.ignoresMouseEvents`）、不可由
/// 背景拖动（`NSWindow.isMovableByWindowBackground`）、三个标准按钮（红点 / 最小化 / 缩放，
/// `NSWindow.standardWindowButton:` + `NSControl.isEnabled`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnerChrome {
    /// `NSWindow.ignoresMouseEvents`：窗口对鼠标透明（点击穿透到下层）。
    pub ignores_mouse_events: bool,
    /// `NSWindow.isMovableByWindowBackground`：窗口内容区可拖动窗口。
    pub movable_by_window_background: bool,
    /// 红点（`NSWindowCloseButton`）是否可点。
    pub close_button_enabled: bool,
    /// 最小化（`NSWindowMiniaturizeButton`）是否可点。
    pub minimize_button_enabled: bool,
    /// 缩放（`NSWindowZoomButton`）是否可点。
    pub zoom_button_enabled: bool,
}

impl OwnerChrome {
    /// 模态期施加到 owner 的拦截态（用户批准方案 A）。
    ///
    /// 与 Windows `EnableWindow(owner, FALSE)` 的效果对齐：owner 的内容区与原生 chrome
    /// （红点 / 标题栏 / 交通灯）都不再接收鼠标交互。关闭时按打开前的快照逐项恢复
    /// （[`release_modal_owner`]），因此这里**不能**用「默认值」当恢复目标。
    pub(crate) const BLOCKED: Self = Self {
        ignores_mouse_events: true,
        movable_by_window_background: false,
        close_button_enabled: false,
        minimize_button_enabled: false,
        zoom_button_enabled: false,
    };
}

/// 模态关闭时对 owner 的收尾决策（RB-07）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModalOwnerRelease {
    /// 要写回 owner 的 chrome（`None` = owner 已不在拦截态，保持现状）。
    pub chrome: Option<OwnerChrome>,
    /// 是否把键盘焦点还给 owner（`NSWindow.makeKeyWindow`）。
    pub return_key: bool,
}

/// 模态关闭时对 owner 应用什么（纯决策；真机侧翻成 AppKit 调用）。
///
/// `current` 是**关闭这一刻**读到的 owner chrome，`original` 是打开模态时的快照：
///
/// - `chrome`：只在 `current == OwnerChrome::BLOCKED` 时恢复快照。若 owner 已经被别的模态
///   窗口恢复过（同一 owner 上多个模态窗口，任意关闭顺序）或应用自己改过 chrome，盲目写
///   快照会把 owner 永久留在拦截态（A 关闭恢复原值 → B 关闭又写回 B 快照到的「拦截态」）。
/// - `return_key`：模态窗口仍持有键盘焦点、且 owner 仍可用（可见、未最小化）时把焦点还给
///   owner —— 与 Windows `EnableWindow(owner, TRUE)` 后焦点回到 owner 的恢复语义对齐。
pub(crate) fn release_modal_owner(
    current: OwnerChrome,
    original: OwnerChrome,
    modal_is_key: bool,
    owner_usable: bool,
) -> ModalOwnerRelease {
    ModalOwnerRelease {
        chrome: (current == OwnerChrome::BLOCKED).then_some(original),
        return_key: modal_is_key && owner_usable,
    }
}

/// 模态窗口每轮是否应从 owner 手里把键盘焦点拿回来（RB-07）。
///
/// owner 被原生拦截后用户点不到它，但 AppKit 仍可能通过窗口循环 / 系统激活把它变成 key
/// window；此时键盘事件进 owner，被引擎的模态输入过滤丢弃 ⇒ **模态窗口收不到键**。泵每轮
/// 用本函数对账，把焦点拿回模态窗口。
pub(crate) const fn modal_takes_key(modal: bool, owner_is_key: bool, modal_is_key: bool) -> bool {
    modal && owner_is_key && !modal_is_key
}

#[cfg(test)]
mod tests {
    use super::{
        ModalOwnerRelease, OwnerChrome, OwnerResolution, WindowId, WindowLevel, modal_takes_key,
        release_modal_owner, resolve_owner, window_level,
    };

    /// 默认（无置顶、无模态）不抬高层级：保持 AppKit 默认，避免无谓的 `setLevel:` 调用。
    #[test]
    fn plain_windows_keep_the_default_level() {
        assert_eq!(window_level(false, false), WindowLevel::Normal);
    }

    /// 置顶 → Floating。
    #[test]
    fn always_on_top_maps_to_floating() {
        assert_eq!(window_level(true, false), WindowLevel::Floating);
    }

    /// 模态 → ModalPanel，且**压过**置顶（模态是 owner 的临时交互面板，必须最高）。
    #[test]
    fn modal_wins_over_always_on_top() {
        assert_eq!(window_level(false, true), WindowLevel::ModalPanel);
        assert_eq!(window_level(true, true), WindowLevel::ModalPanel);
    }

    /// owner 解析：存在 ⇒ 挂接；不存在/自指 ⇒ 降级 + 标记 Missing。
    #[test]
    fn owner_resolution_falls_back_to_none_when_the_native_window_is_missing() {
        let owner = WindowId::from_raw(1);
        let owned = WindowId::from_raw(2);
        assert_eq!(
            resolve_owner(owned, Some(owner), |candidate| candidate == owner),
            OwnerResolution::Attach(owner)
        );
        assert_eq!(
            resolve_owner(owned, Some(WindowId::from_raw(9)), |candidate| {
                candidate == owner
            }),
            OwnerResolution::Missing
        );
        // 自指：窗口不能是自己的 child。
        assert_eq!(
            resolve_owner(owned, Some(owned), |_| true),
            OwnerResolution::Missing
        );
        // 未请求 owner：不写诊断。
        assert_eq!(resolve_owner(owned, None, |_| true), OwnerResolution::None);
    }

    // ── RB-07：模态窗口对 owner 的原生 chrome 拦截 ──────────────────────────────

    /// owner 打开前的 chrome（刻意**不是**默认值：应用自己禁用过红点、开启过背景拖动）。
    const APP_CHROME: OwnerChrome = OwnerChrome {
        ignores_mouse_events: false,
        movable_by_window_background: true,
        close_button_enabled: false,
        minimize_button_enabled: true,
        zoom_button_enabled: true,
    };

    /// 模态期 owner 的 chrome 必须全部关闭：鼠标透明、不可背景拖动、三个标准按钮禁用
    /// （与 Windows `EnableWindow(owner, FALSE)` 的语义对齐）。
    #[test]
    fn the_blocked_owner_chrome_disables_every_native_interaction() {
        let blocked = OwnerChrome::BLOCKED;
        assert!(
            blocked.ignores_mouse_events,
            "模态期 owner 必须对鼠标透明（点 owner 不得抢 key、红点不得可点）"
        );
        assert!(
            !blocked.movable_by_window_background,
            "模态期 owner 不得由背景拖动"
        );
        assert!(!blocked.close_button_enabled, "模态期红点必须禁用");
        assert!(!blocked.minimize_button_enabled, "模态期最小化必须禁用");
        assert!(!blocked.zoom_button_enabled, "模态期缩放必须禁用");
    }

    /// 恢复必须回到**打开前的原值**（不能假设默认值）：应用自己禁用过的红点、开启过的背景
    /// 拖动都要原样还回去。
    #[test]
    fn releasing_a_modal_owner_restores_the_snapshot_not_the_defaults() {
        let release = release_modal_owner(OwnerChrome::BLOCKED, APP_CHROME, true, true);
        assert_eq!(
            release.chrome,
            Some(APP_CHROME),
            "关闭时必须逐项写回打开前的快照"
        );
        assert!(
            release.return_key,
            "模态持有 key 且 owner 可用 ⇒ 焦点还给 owner"
        );
    }

    /// 恢复是幂等的：第二次释放看到 owner 已不在拦截态 ⇒ 保持现状，不把拦截态写回去。
    #[test]
    fn releasing_twice_keeps_the_restored_chrome() {
        let first = release_modal_owner(OwnerChrome::BLOCKED, APP_CHROME, true, true);
        let restored = first.chrome.expect("第一次释放必须恢复");
        assert_eq!(restored, APP_CHROME);
        let second = release_modal_owner(restored, APP_CHROME, true, true);
        assert_eq!(second.chrome, None, "已恢复的 chrome 不得再被写一遍");
    }

    /// 同一 owner 上的两个模态窗口：**任意关闭顺序**最终都回到打开前的原值，不会把 owner
    /// 永久留在拦截态（B 的快照本身就是拦截态，盲目写回会永久禁用 owner）。
    #[test]
    fn two_modal_windows_on_one_owner_never_leave_it_blocked() {
        // 模拟真机侧：open 快照并施加 BLOCKED，close 按决策恢复。
        fn open(owner: &mut OwnerChrome) -> OwnerChrome {
            let snapshot = *owner;
            *owner = OwnerChrome::BLOCKED;
            snapshot
        }
        fn close(owner: &mut OwnerChrome, snapshot: OwnerChrome) {
            let release: ModalOwnerRelease = release_modal_owner(*owner, snapshot, true, true);
            if let Some(chrome) = release.chrome {
                *owner = chrome;
            }
        }

        // 顺序 1：A 先开、B 后开；先关 A 再关 B（非 LIFO，最坏情况）。
        let mut owner = APP_CHROME;
        let a = open(&mut owner);
        let b = open(&mut owner);
        close(&mut owner, a);
        close(&mut owner, b);
        assert_eq!(owner, APP_CHROME, "先关旧模态也不得永久拦截 owner");

        // 顺序 2：LIFO 关闭（B 先关）。
        let mut owner = APP_CHROME;
        let a = open(&mut owner);
        let b = open(&mut owner);
        close(&mut owner, b);
        close(&mut owner, a);
        assert_eq!(owner, APP_CHROME);
    }

    /// 模态窗口必须从 owner 手里拿回键盘焦点：owner 抢到 key 后键盘事件进 owner，会被引擎的
    /// 模态输入过滤丢弃 —— 模态窗口就收不到键了（RB-07 最严重的一条）。
    #[test]
    fn a_modal_window_reclaims_the_key_from_its_owner() {
        assert!(
            modal_takes_key(true, true, false),
            "owner 抢到 key、模态不是 key ⇒ 模态必须拿回焦点"
        );
        assert!(
            !modal_takes_key(true, false, false),
            "owner 不是 key ⇒ 不动焦点"
        );
        assert!(
            !modal_takes_key(true, true, true),
            "模态已是 key ⇒ 不重复抢"
        );
        assert!(
            !modal_takes_key(false, true, false),
            "非模态窗口不抢 owner 的焦点"
        );
    }
}

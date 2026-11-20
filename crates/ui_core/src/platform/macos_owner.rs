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
//! 的**原值**快照下来，关闭时按 [`ModalOwnerChromes`] 的派生判据逐项恢复（不假设默认值）。
//! 模态窗口显示时 `makeKeyAndOrderFront:` 接管键盘焦点，泵每轮再用 [`modal_takes_key`]
//! 把被 AppKit 窗口循环抢回 owner 的 key 拿回来；关闭时把 key 还给仍可用的 owner。
//!
//! # 同一 owner 多个模态窗口（RB-13 的 mac 同类）
//!
//! 恢复条件不是「关闭了一个模态窗口」，而是**派生值**：该 owner 已没有任何活着的模态窗口
//! （[`ModalOwnerChromes`] 维护「仍活着的模态窗口集合」，在 `NativeWindow` 的建窗 / 关窗
//! 两处登记与注销）。只关掉第一个模态窗口时 owner 的 chrome **保持拦截**，焦点交给另一个
//! 仍活着的模态窗口 —— 与 Windows `EnableWindow(owner, FALSE)` 在多模态下的目标语义一致
//! （Windows 侧 RB-13 的计数缺陷另行修复）。快照只由第一个模态窗口记录、由最后一个关闭的
//! 窗口恢复，避免第二个窗口把「拦截态」当成原值写回。
//!
//! **不用** `beginSheet:`（会改变窗口形态、把模态窗口嵌进 owner 的标题栏）与
//! `runModalForWindow:`（嵌套 run loop 与引擎泵冲突）。

use std::collections::hash_map::Entry;

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

/// 模态窗口关闭后对 owner 的收尾决策（RB-07；RB-13 的 mac 同类）。
///
/// 判据是**派生值**：owner 上还有没有活着的模态窗口（见 [`ModalOwnerChromes`]），不是
/// +/-1 计数 —— 计数在异常收尾路径上会漂移，集合则与窗口对象的生命周期同源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModalOwnerRelease {
    /// owner 已没有任何模态窗口：按打开前的快照恢复 chrome；`return_key` = 关闭的是 key
    /// 窗口且 owner 仍可用 ⇒ 把键盘焦点还给 owner（与 Windows `EnableWindow(owner, TRUE)`
    /// 后焦点回到 owner 的恢复语义对齐）。
    Last {
        chrome: OwnerChrome,
        return_key: bool,
    },
    /// owner 仍有其它活着的模态窗口：chrome **保持拦截**（不能提前恢复，否则原生/引擎语义
    /// 分叉 —— 引擎层仍在拦输入，owner 的原生 chrome 却可点了）。`hand_key` = 关闭的是 key
    /// 窗口 ⇒ 焦点交给另一个仍活着的模态窗口（**不是** owner）。
    Remaining { hand_key: bool },
    /// 本窗口没有登记（不是模态窗口 / owner 未解析 / 已经释放过）：什么都不做。
    None,
}

/// 一个 owner 的模态拦截记账：**仍活着的模态窗口** + 打开前的 chrome 快照。
///
/// # 恢复判据是派生值，不是计数器
///
/// 同一 owner 上可以有多个模态窗口（API 允许）。恢复 owner 原生交互的条件必须是
/// 「该 owner 已没有任何活着的模态窗口」，而不是「关闭了一个模态窗口」：否则关掉第一个就
/// 恢复，与引擎层仍按 owner 关系拦截输入的事实分叉（Windows RB-13 的同类缺陷）。
///
/// 这里用 `modals: Vec<WindowId>` 表达「仍活着的模态窗口集合」，在**建窗 / 关窗**两处维护；
/// 关窗时按集合的**当前内容**派生收尾动作。窗口对象的唯一所有者仍是 macOS 事件循环的
/// `LoopState.windows`：任何收尾路径（引擎关闭 / 外部关闭 / 级联关闭 / App 关闭 / Drop）
/// 都会走到 `NativeWindow::close` 的注销，因此集合不会因异常路径永久漂移。
///
/// 打开前的 chrome 快照只由**第一个**模态窗口记录、由**最后一个**关闭的模态窗口恢复：
/// 第二个及之后的窗口若重新快照，会把拦截态当成原值写回（owner 永久不可交互）。
#[derive(Debug, Default)]
pub(crate) struct ModalOwnerChromes {
    owners: ahash::HashMap<WindowId, OwnerModalWindows>,
}

/// 单个 owner 的模态记账（`ModalOwnerChromes` 的内部条目）。
#[derive(Debug)]
struct OwnerModalWindows {
    /// 打开前的 owner chrome（第一个模态窗口记录；最后一个模态窗口恢复）。
    original: OwnerChrome,
    /// 仍活着的模态窗口（非空；最后一个关闭时整条记录移除）。
    modals: Vec<WindowId>,
}

impl ModalOwnerChromes {
    /// 模态窗口打开（唯一写点之一：`NativeWindow::apply_owner` 接线 owner 时）。
    ///
    /// 返回 `true` = 这是该 owner 的**第一个**模态窗口：快照已记录，调用方应施加
    /// [`OwnerChrome::BLOCKED`]；返回 `false` = 该 owner 已被其它模态窗口拦截，chrome 保持
    /// 现状（不重复快照、不重复施加）。
    pub(crate) fn open(&mut self, modal: WindowId, owner: WindowId, observed: OwnerChrome) -> bool {
        match self.owners.entry(owner) {
            Entry::Occupied(mut entry) => {
                let windows = entry.get_mut();
                // 同一窗口重复 open（防御：接线只有一次）不得把集合写重。
                if !windows.modals.contains(&modal) {
                    windows.modals.push(modal);
                }
                false
            }
            Entry::Vacant(entry) => {
                entry.insert(OwnerModalWindows {
                    original: observed,
                    modals: vec![modal],
                });
                true
            }
        }
    }

    /// 模态窗口关闭（唯一写点之二：`NativeWindow::restore_modal_owner`，被 close /
    /// 外部关闭 / Drop 共用）。
    ///
    /// 从活集合移除后按**当前内容**派生：集合空 ⇒ 恢复快照 + 按需归还 owner 焦点；集合
    /// 非空 ⇒ 保持拦截 + 按需把焦点交给另一个仍活着的模态窗口。
    pub(crate) fn close(
        &mut self,
        modal: WindowId,
        modal_is_key: bool,
        owner_usable: bool,
    ) -> ModalOwnerRelease {
        // 按窗口 id 找它所属的 owner 条目：模态窗口数量是个位数，线性扫描即可；不引入
        // 「模态 → owner」的第二张表（多一个需要同步的真值来源）。
        let Some((owner, windows)) = self
            .owners
            .iter_mut()
            .find(|(_, windows)| windows.modals.contains(&modal))
        else {
            return ModalOwnerRelease::None;
        };
        let owner = *owner;
        let Some(index) = windows
            .modals
            .iter()
            .position(|candidate| *candidate == modal)
        else {
            return ModalOwnerRelease::None;
        };
        windows.modals.swap_remove(index);
        if !windows.modals.is_empty() {
            return ModalOwnerRelease::Remaining {
                hand_key: modal_is_key,
            };
        }
        let original = windows.original;
        self.owners.remove(&owner);
        ModalOwnerRelease::Last {
            chrome: original,
            return_key: modal_is_key && owner_usable,
        }
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
        ModalOwnerChromes, ModalOwnerRelease, OwnerChrome, OwnerResolution, WindowId, WindowLevel,
        modal_takes_key, resolve_owner, window_level,
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
    /// 拖动都要原样还回去；单个模态窗口关闭时同时把焦点还给 owner。
    #[test]
    fn the_last_modal_window_restores_the_snapshot_and_the_owner_key() {
        let owner = WindowId::from_raw(1);
        let modal = WindowId::from_raw(2);
        let mut chromes = ModalOwnerChromes::default();
        assert!(
            chromes.open(modal, owner, APP_CHROME),
            "第一个模态窗口必须快照"
        );
        assert_eq!(
            chromes.close(modal, true, true),
            ModalOwnerRelease::Last {
                chrome: APP_CHROME,
                return_key: true,
            },
            "最后一个模态窗口关闭：按快照恢复 chrome，并把焦点还给 owner"
        );
        assert_eq!(
            chromes.close(modal, true, true),
            ModalOwnerRelease::None,
            "重复关闭不得再恢复一次（幂等）"
        );
    }

    /// owner 不可用（已关闭 / 最小化）时 chrome 仍要恢复，但不得抢键盘焦点。
    #[test]
    fn a_closed_owner_still_gets_its_chrome_back_without_the_key() {
        let owner = WindowId::from_raw(1);
        let modal = WindowId::from_raw(2);
        let mut chromes = ModalOwnerChromes::default();
        assert!(chromes.open(modal, owner, APP_CHROME));
        assert_eq!(
            chromes.close(modal, true, false),
            ModalOwnerRelease::Last {
                chrome: APP_CHROME,
                return_key: false,
            }
        );
    }

    /// **RB-13 的 mac 同类核心回归**：同一 owner 上两个模态窗口，关掉第一个**不得**恢复
    /// owner 的 chrome —— 引擎层仍按 owner 关系拦截输入，提前恢复原生交互会造成语义分叉；
    /// 关闭的是 key 窗口时焦点应交给**另一个仍活着的模态窗口**，不是 owner。
    /// 只有最后一个模态窗口关闭才恢复打开前的快照并把焦点还给 owner。
    #[test]
    fn closing_the_first_of_two_modal_windows_keeps_the_owner_blocked() {
        let owner = WindowId::from_raw(1);
        let first = WindowId::from_raw(2);
        let second = WindowId::from_raw(3);

        // 顺序 1（FIFO）：先关第一个。
        let mut chromes = ModalOwnerChromes::default();
        assert!(chromes.open(first, owner, APP_CHROME));
        assert!(
            !chromes.open(second, owner, OwnerChrome::BLOCKED),
            "第二个模态窗口不得把拦截态重新快照成原值"
        );
        assert_eq!(
            chromes.close(first, true, true),
            ModalOwnerRelease::Remaining { hand_key: true },
            "仍有模态窗口：owner chrome 必须保持 blocked，key 交给另一个模态窗口"
        );
        assert_eq!(
            chromes.close(second, true, true),
            ModalOwnerRelease::Last {
                chrome: APP_CHROME,
                return_key: true,
            },
            "最后一个关闭：恢复打开前的快照并把 key 还给 owner"
        );

        // 顺序 2（LIFO）：先关第二个。中间态同样必须保持拦截。
        let mut chromes = ModalOwnerChromes::default();
        assert!(chromes.open(first, owner, APP_CHROME));
        assert!(!chromes.open(second, owner, OwnerChrome::BLOCKED));
        assert_eq!(
            chromes.close(second, true, true),
            ModalOwnerRelease::Remaining { hand_key: true }
        );
        assert_eq!(
            chromes.close(first, true, true),
            ModalOwnerRelease::Last {
                chrome: APP_CHROME,
                return_key: true,
            }
        );
    }

    /// 关闭的不是 key 模态窗口时不得抢焦点（owner 与其它模态窗口都不动），但拦截/恢复判据
    /// 不变。
    #[test]
    fn closing_a_non_key_modal_window_never_steals_the_key() {
        let owner = WindowId::from_raw(1);
        let first = WindowId::from_raw(2);
        let second = WindowId::from_raw(3);
        let mut chromes = ModalOwnerChromes::default();
        assert!(chromes.open(first, owner, APP_CHROME));
        assert!(!chromes.open(second, owner, OwnerChrome::BLOCKED));
        assert_eq!(
            chromes.close(first, false, true),
            ModalOwnerRelease::Remaining { hand_key: false }
        );
        assert_eq!(
            chromes.close(second, false, true),
            ModalOwnerRelease::Last {
                chrome: APP_CHROME,
                return_key: false,
            }
        );
    }

    /// 未登记的窗口关闭是 no-op（不是模态窗口 / owner 未解析 / 已释放）。
    #[test]
    fn closing_an_unregistered_window_is_a_noop() {
        let owner = WindowId::from_raw(1);
        let mut chromes = ModalOwnerChromes::default();
        assert_eq!(
            chromes.close(WindowId::from_raw(9), true, true),
            ModalOwnerRelease::None
        );
        assert!(chromes.open(WindowId::from_raw(2), owner, APP_CHROME));
        assert_eq!(
            chromes.close(WindowId::from_raw(9), true, true),
            ModalOwnerRelease::None,
            "别的窗口关闭不得动这个 owner 的记账"
        );
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

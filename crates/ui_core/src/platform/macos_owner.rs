use std::collections::hash_map::Entry;

use crate::window::WindowId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowLevel {
    Normal,
    Floating,
    ModalPanel,
}

pub(crate) const fn window_level(always_on_top: bool, modal: bool) -> WindowLevel {
    if modal {
        WindowLevel::ModalPanel
    } else if always_on_top {
        WindowLevel::Floating
    } else {
        WindowLevel::Normal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerResolution {
    Attach(WindowId),
    None,
    Missing,
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnerChrome {
    pub ignores_mouse_events: bool,
    pub movable_by_window_background: bool,
    pub close_button_enabled: bool,
    pub minimize_button_enabled: bool,
    pub zoom_button_enabled: bool,
}

impl OwnerChrome {
    pub(crate) const BLOCKED: Self = Self {
        ignores_mouse_events: true,
        movable_by_window_background: false,
        close_button_enabled: false,
        minimize_button_enabled: false,
        zoom_button_enabled: false,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModalOwnerRelease {
    Last {
        chrome: OwnerChrome,
        return_key: bool,
    },
    Remaining {
        hand_key: bool,
    },
    None,
}

#[derive(Debug, Default)]
pub(crate) struct ModalOwnerChromes {
    owners: ahash::HashMap<WindowId, OwnerModalWindows>,
}

#[derive(Debug)]
struct OwnerModalWindows {
    original: OwnerChrome,
    modals: Vec<WindowId>,
}

impl ModalOwnerChromes {
    pub(crate) fn open(&mut self, modal: WindowId, owner: WindowId, observed: OwnerChrome) -> bool {
        match self.owners.entry(owner) {
            Entry::Occupied(mut entry) => {
                let windows = entry.get_mut();
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

    pub(crate) fn close(
        &mut self,
        modal: WindowId,
        modal_is_key: bool,
        owner_usable: bool,
    ) -> ModalOwnerRelease {
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

pub(crate) const fn modal_takes_key(modal: bool, owner_is_key: bool, modal_is_key: bool) -> bool {
    modal && owner_is_key && !modal_is_key
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowKeyInput {
    pub window: WindowId,
    pub owner: Option<WindowId>,
    pub modal: bool,
    pub is_key: bool,
}

pub(crate) fn reclaim_modal_keys(windows: &[WindowKeyInput]) -> Option<WindowId> {
    let is_key = |id: WindowId| {
        windows
            .iter()
            .any(|candidate| candidate.window == id && candidate.is_key)
    };
    windows.iter().find_map(|state| {
        let owner_is_key = state.owner.is_some_and(is_key);
        modal_takes_key(state.modal, owner_is_key, state.is_key).then_some(state.window)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ModalOwnerChromes, ModalOwnerRelease, OwnerChrome, OwnerResolution, WindowId,
        WindowKeyInput, WindowLevel, modal_takes_key, reclaim_modal_keys, resolve_owner,
        window_level,
    };

    #[test]
    fn plain_windows_keep_the_default_level() {
        assert_eq!(window_level(false, false), WindowLevel::Normal);
    }

    #[test]
    fn always_on_top_maps_to_floating() {
        assert_eq!(window_level(true, false), WindowLevel::Floating);
    }

    #[test]
    fn modal_wins_over_always_on_top() {
        assert_eq!(window_level(false, true), WindowLevel::ModalPanel);
        assert_eq!(window_level(true, true), WindowLevel::ModalPanel);
    }

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
        assert_eq!(
            resolve_owner(owned, Some(owned), |_| true),
            OwnerResolution::Missing
        );
        assert_eq!(resolve_owner(owned, None, |_| true), OwnerResolution::None);
    }

    const APP_CHROME: OwnerChrome = OwnerChrome {
        ignores_mouse_events: false,
        movable_by_window_background: true,
        close_button_enabled: false,
        minimize_button_enabled: true,
        zoom_button_enabled: true,
    };

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

    #[test]
    fn closing_the_first_of_two_modal_windows_keeps_the_owner_blocked() {
        let owner = WindowId::from_raw(1);
        let first = WindowId::from_raw(2);
        let second = WindowId::from_raw(3);

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

    #[test]
    fn the_key_prescan_prevents_the_owner_from_being_key_during_the_snapshot_phase() {
        let owner = WindowId::from_raw(1);
        let modal = WindowId::from_raw(2);
        let mut windows = [
            WindowKeyInput {
                window: owner,
                owner: None,
                modal: false,
                is_key: true,
            },
            WindowKeyInput {
                window: modal,
                owner: Some(owner),
                modal: true,
                is_key: false,
            },
        ];

        let reclaim = reclaim_modal_keys(&windows);
        assert_eq!(
            reclaim,
            Some(modal),
            "预扫描必须识别出「owner 抢到 key、模态不是 key」并交给模态窗口"
        );
        if let Some(modal_id) = reclaim {
            for state in &mut windows {
                state.is_key = state.window == modal_id;
            }
        }

        let owner_state = windows
            .iter()
            .find(|state| state.window == owner)
            .expect("owner 窗口");
        assert!(
            !owner_state.is_key,
            "预扫描之后 owner 在快照阶段必须看到 is_key=false（否则会投递假 FocusChanged{{true}} 并触发多余 IME 清场）"
        );
        assert!(
            windows
                .iter()
                .find(|state| state.window == modal)
                .expect("模态窗口")
                .is_key,
            "模态窗口保持 key"
        );
    }

    #[test]
    fn the_key_prescan_leaves_ordinary_focus_alone() {
        let owner = WindowId::from_raw(1);
        let modal = WindowId::from_raw(2);
        let other = WindowId::from_raw(3);
        let windows = [
            WindowKeyInput {
                window: owner,
                owner: None,
                modal: false,
                is_key: false,
            },
            WindowKeyInput {
                window: modal,
                owner: Some(owner),
                modal: true,
                is_key: false,
            },
            WindowKeyInput {
                window: other,
                owner: None,
                modal: false,
                is_key: true,
            },
        ];
        assert_eq!(
            reclaim_modal_keys(&windows),
            None,
            "key 在无关窗口上时不得抢焦点"
        );
        let windows = [
            WindowKeyInput {
                window: owner,
                owner: None,
                modal: false,
                is_key: false,
            },
            WindowKeyInput {
                window: modal,
                owner: Some(owner),
                modal: true,
                is_key: true,
            },
        ];
        assert_eq!(reclaim_modal_keys(&windows), None);
    }

    #[test]
    fn two_owners_keep_independent_modal_ledgers() {
        let owner_a = WindowId::from_raw(1);
        let owner_b = WindowId::from_raw(2);
        let modal_a = WindowId::from_raw(3);
        let modal_b = WindowId::from_raw(4);
        let chrome_a = OwnerChrome {
            ignores_mouse_events: false,
            movable_by_window_background: true,
            close_button_enabled: false,
            minimize_button_enabled: true,
            zoom_button_enabled: true,
        };
        let chrome_b = OwnerChrome {
            ignores_mouse_events: false,
            movable_by_window_background: false,
            close_button_enabled: true,
            minimize_button_enabled: false,
            zoom_button_enabled: true,
        };
        let mut chromes = ModalOwnerChromes::default();
        assert!(chromes.open(modal_a, owner_a, chrome_a));
        assert!(chromes.open(modal_b, owner_b, chrome_b));
        assert_eq!(
            chromes.close(modal_a, true, true),
            ModalOwnerRelease::Last {
                chrome: chrome_a,
                return_key: true,
            }
        );
        assert_eq!(
            chromes.close(modal_b, true, true),
            ModalOwnerRelease::Last {
                chrome: chrome_b,
                return_key: true,
            }
        );
    }

    #[test]
    fn opening_the_same_modal_window_twice_does_not_duplicate_it() {
        let owner = WindowId::from_raw(1);
        let modal = WindowId::from_raw(2);
        let mut chromes = ModalOwnerChromes::default();
        assert!(chromes.open(modal, owner, APP_CHROME));
        assert!(
            !chromes.open(modal, owner, OwnerChrome::BLOCKED),
            "重复 open 不得再次快照/施加拦截"
        );
        assert_eq!(
            chromes.close(modal, true, true),
            ModalOwnerRelease::Last {
                chrome: APP_CHROME,
                return_key: true,
            },
            "重复 open 不得让集合残留第二个条目（否则关一次仍判 Remaining）"
        );
    }

    #[test]
    fn an_unavailable_owner_keeps_the_intermediate_modal_state() {
        let owner = WindowId::from_raw(1);
        let first = WindowId::from_raw(2);
        let second = WindowId::from_raw(3);
        let mut chromes = ModalOwnerChromes::default();
        assert!(chromes.open(first, owner, APP_CHROME));
        assert!(!chromes.open(second, owner, OwnerChrome::BLOCKED));
        assert_eq!(
            chromes.close(first, true, false),
            ModalOwnerRelease::Remaining { hand_key: true },
            "owner 不可用不影响「仍有模态窗口 ⇒ 保持拦截」"
        );
        assert_eq!(
            chromes.close(second, true, false),
            ModalOwnerRelease::Last {
                chrome: APP_CHROME,
                return_key: false,
            },
            "最后一个关闭仍恢复 chrome，但不得把焦点还给不可用的 owner"
        );
    }
}

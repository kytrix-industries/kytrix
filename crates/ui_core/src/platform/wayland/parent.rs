use crate::window::WindowId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParentAction {
    SendNow,
    Deferred,
}

#[derive(Default)]
pub(crate) struct ParentHints {
    pending: Vec<(WindowId, WindowId)>,
}

impl ParentHints {
    pub(crate) fn note(
        &mut self,
        child: WindowId,
        owner: WindowId,
        owner_mapped: bool,
    ) -> ParentAction {
        if owner_mapped {
            ParentAction::SendNow
        } else {
            self.pending.push((child, owner));
            ParentAction::Deferred
        }
    }

    pub(crate) fn take_children_of(&mut self, owner: WindowId) -> Vec<WindowId> {
        let mut children = Vec::with_capacity(self.pending.len());
        self.pending.retain(|(child, pending_owner)| {
            if *pending_owner == owner {
                children.push(*child);
                false
            } else {
                true
            }
        });
        children
    }

    pub(crate) fn forget(&mut self, window: WindowId) {
        self.pending
            .retain(|(child, owner)| *child != window && *owner != window);
    }
}

#[cfg(test)]
mod tests {
    use super::{ParentAction, ParentHints};
    use crate::window::WindowId;

    fn window(id: u32) -> WindowId {
        WindowId::for_test(id)
    }

    #[test]
    fn an_unmapped_owner_defers_the_hint_until_it_maps() {
        let mut hints = ParentHints::default();
        let child = window(2);
        let owner = window(1);
        assert_eq!(
            hints.note(child, owner, false),
            ParentAction::Deferred,
            "owner 未映射时立刻发 set_parent 等于 null parent，提示会丢"
        );
        assert_eq!(
            hints.take_children_of(owner),
            vec![child],
            "owner 映射后必须把等待中的 child 交回调用方补发"
        );
    }

    #[test]
    fn a_mapped_owner_sends_the_hint_immediately() {
        let mut hints = ParentHints::default();
        assert_eq!(
            hints.note(window(2), window(1), true),
            ParentAction::SendNow,
            "owner 已映射 ⇒ 当场发 set_parent"
        );
        assert!(
            hints.take_children_of(window(1)).is_empty(),
            "已经发过的提示不得再进补发队列（否则会重复 set_parent）"
        );
    }

    #[test]
    fn deferred_hints_are_flushed_exactly_once() {
        let mut hints = ParentHints::default();
        let child = window(2);
        let owner = window(1);
        assert_eq!(hints.note(child, owner, false), ParentAction::Deferred);

        assert_eq!(hints.take_children_of(owner), vec![child]);
        assert!(
            hints.take_children_of(owner).is_empty(),
            "同一次 owner 映射只补发一次，第二次必须为空"
        );
    }

    #[test]
    fn hints_are_flushed_in_registration_order() {
        let mut hints = ParentHints::default();
        let owner = window(1);
        for child in [window(3), window(2), window(4)] {
            assert_eq!(hints.note(child, owner, false), ParentAction::Deferred);
        }
        assert_eq!(
            hints.take_children_of(owner),
            vec![window(3), window(2), window(4)],
            "补发顺序 = 登记顺序"
        );
    }

    #[test]
    fn flushing_one_owner_leaves_other_owners_pending() {
        let mut hints = ParentHints::default();
        hints.note(window(2), window(1), false);
        hints.note(window(4), window(3), false);

        assert_eq!(hints.take_children_of(window(1)), vec![window(2)]);
        assert_eq!(
            hints.take_children_of(window(3)),
            vec![window(4)],
            "另一个 owner 的等待项必须保留"
        );
    }

    #[test]
    fn a_destroyed_owner_drops_its_pending_hints() {
        let mut hints = ParentHints::default();
        let child = window(2);
        let owner = window(1);
        hints.note(child, owner, false);

        hints.forget(owner);
        assert!(
            hints.take_children_of(owner).is_empty(),
            "owner 已关闭 ⇒ 不得再补发 set_parent"
        );
    }

    #[test]
    fn a_destroyed_child_drops_its_pending_hint() {
        let mut hints = ParentHints::default();
        let child = window(2);
        let owner = window(1);
        hints.note(child, owner, false);

        hints.forget(child);
        assert!(
            hints.take_children_of(owner).is_empty(),
            "child 已关闭 ⇒ 不得再对不存在的 toplevel 发 set_parent"
        );
    }

    #[test]
    fn forgetting_one_window_keeps_unrelated_hints() {
        let mut hints = ParentHints::default();
        hints.note(window(2), window(1), false);
        hints.note(window(4), window(3), false);

        hints.forget(window(2));
        assert!(hints.take_children_of(window(1)).is_empty());
        assert_eq!(
            hints.take_children_of(window(3)),
            vec![window(4)],
            "无关的 owner/child 组合不得被误删"
        );
    }
}

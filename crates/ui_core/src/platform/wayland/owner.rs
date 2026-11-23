use crate::window::WindowId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerResolution {
    Unparented,
    Existing(usize),
    Missing(WindowId),
}

#[must_use]
pub(crate) fn resolve_owner(
    owner: Option<WindowId>,
    lookup: impl Fn(WindowId) -> Option<usize>,
) -> OwnerResolution {
    let Some(owner) = owner else {
        return OwnerResolution::Unparented;
    };
    match lookup(owner) {
        Some(index) => OwnerResolution::Existing(index),
        None => OwnerResolution::Missing(owner),
    }
}

#[cfg(test)]
mod tests {
    use super::{OwnerResolution, resolve_owner};
    use crate::window::WindowId;

    fn windows() -> [WindowId; 3] {
        [
            WindowId::for_test(1),
            WindowId::for_test(2),
            WindowId::for_test(3),
        ]
    }

    fn lookup(owner: WindowId) -> Option<usize> {
        windows().iter().position(|id| *id == owner)
    }

    #[test]
    fn no_owner_is_unparented() {
        assert_eq!(
            resolve_owner(None, lookup),
            OwnerResolution::Unparented,
            "未声明 owner 不得设 parent"
        );
    }

    #[test]
    fn an_existing_owner_resolves_to_its_window_index() {
        assert_eq!(
            resolve_owner(Some(WindowId::for_test(1)), lookup),
            OwnerResolution::Existing(0)
        );
        assert_eq!(
            resolve_owner(Some(WindowId::for_test(2)), lookup),
            OwnerResolution::Existing(1)
        );
        assert_eq!(
            resolve_owner(Some(WindowId::for_test(3)), lookup),
            OwnerResolution::Existing(2)
        );
    }

    #[test]
    fn a_missing_owner_degrades_to_missing() {
        for stale in [4, 99, u32::MAX] {
            assert_eq!(
                resolve_owner(Some(WindowId::for_test(stale)), lookup),
                OwnerResolution::Missing(WindowId::for_test(stale)),
                "不存在的 owner {stale} 必须降级为 Missing"
            );
        }
        assert_ne!(
            resolve_owner(Some(WindowId::for_test(4)), lookup),
            OwnerResolution::Existing(0)
        );
    }

    #[test]
    fn a_self_reference_is_missing_before_the_window_is_pushed() {
        let new_window = WindowId::for_test(7);
        assert_eq!(
            resolve_owner(Some(new_window), lookup),
            OwnerResolution::Missing(new_window)
        );
    }

    #[test]
    fn the_index_is_the_position_in_the_window_list() {
        let ids = [WindowId::for_test(7), WindowId::for_test(3)];
        let lookup = |owner: WindowId| ids.iter().position(|id| *id == owner);
        assert_eq!(
            resolve_owner(Some(WindowId::for_test(3)), lookup),
            OwnerResolution::Existing(1)
        );
    }
}

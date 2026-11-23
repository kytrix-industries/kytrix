use crate::event::{KeyState, Modifiers, PointerButton};

#[must_use]
pub(crate) const fn pointer_button(button: u32) -> Option<PointerButton> {
    match button {
        0x110 => Some(PointerButton::Primary),
        0x111 => Some(PointerButton::Secondary),
        0x112 => Some(PointerButton::Middle),
        0x113 => Some(PointerButton::X1),
        0x114 => Some(PointerButton::X2),
        _ => None,
    }
}

#[must_use]
pub(crate) fn key_visible_while_composing(modifiers: Modifiers, composing: bool) -> bool {
    crate::platform::ime::key_visible(
        false,
        composing,
        crate::platform::wayland::text_input::text_blocked_by_modifiers(modifiers),
    )
}

#[must_use]
pub(crate) fn key_delivered(state: KeyState, modifiers: Modifiers, composing: bool) -> bool {
    matches!(state, KeyState::Up) || key_visible_while_composing(modifiers, composing)
}

#[cfg(test)]
mod tests {
    use super::{key_delivered, key_visible_while_composing, pointer_button};
    use crate::event::{KeyState, Modifiers, PointerButton};

    #[test]
    fn the_pointer_button_table_matches_evdev() {
        assert_eq!(pointer_button(0x110), Some(PointerButton::Primary));
        assert_eq!(pointer_button(0x111), Some(PointerButton::Secondary));
        assert_eq!(pointer_button(0x112), Some(PointerButton::Middle));
        assert_eq!(pointer_button(0x113), Some(PointerButton::X1));
        assert_eq!(pointer_button(0x114), Some(PointerButton::X2));
    }

    #[test]
    fn btn_right_is_secondary_and_btn_middle_is_middle() {
        assert_eq!(
            pointer_button(0x111),
            Some(PointerButton::Secondary),
            "BTN_RIGHT(0x111) 是右键（Windows WM_RBUTTONDOWN / mac rightMouseDown:）"
        );
        assert_eq!(
            pointer_button(0x112),
            Some(PointerButton::Middle),
            "BTN_MIDDLE(0x112) 是中键（Windows WM_MBUTTONDOWN / mac otherMouseDown:）"
        );
        assert_ne!(pointer_button(0x111), Some(PointerButton::Middle));
        assert_ne!(pointer_button(0x112), Some(PointerButton::Secondary));
    }

    #[test]
    fn side_buttons_keep_their_order() {
        assert_eq!(pointer_button(0x113), Some(PointerButton::X1));
        assert_eq!(pointer_button(0x114), Some(PointerButton::X2));
        assert_ne!(pointer_button(0x113), pointer_button(0x114));
    }

    #[test]
    fn unmapped_buttons_are_dropped() {
        for button in [
            0x0,
            0x100,
            0x10f,
            0x115,
            0x116,
            0x120,
            0x14a,
            0x1ff,
            u32::MAX,
        ] {
            assert_eq!(pointer_button(button), None, "码 {button:#x} 不得映射");
        }
    }

    #[test]
    fn a_composing_key_is_hidden_without_command_modifiers() {
        for modifiers in [Modifiers::empty(), Modifiers::SHIFT] {
            assert!(
                !key_visible_while_composing(modifiers, true),
                "组合期（已有 preedit）的按键不得投递裸 Key（modifiers={modifiers:?}）"
            );
        }
    }

    #[test]
    fn a_command_modifier_delivers_while_composing() {
        for modifiers in [
            Modifiers::CONTROL,
            Modifiers::ALT,
            Modifiers::META,
            Modifiers::CONTROL | Modifiers::SHIFT,
        ] {
            assert!(
                key_visible_while_composing(modifiers, true),
                "命令修饰键在组合期也必须投递（modifiers={modifiers:?}）"
            );
        }
    }

    #[test]
    fn an_ordinary_key_is_visible_outside_composition() {
        assert!(key_visible_while_composing(Modifiers::empty(), false));
        assert!(key_visible_while_composing(Modifiers::SHIFT, false));
        assert!(key_visible_while_composing(Modifiers::CONTROL, false));
    }

    #[test]
    fn a_composing_key_up_is_still_delivered() {
        for modifiers in [
            Modifiers::empty(),
            Modifiers::SHIFT,
            Modifiers::CONTROL,
            Modifiers::ALT | Modifiers::SHIFT,
        ] {
            assert!(
                key_delivered(KeyState::Up, modifiers, true),
                "组合期抬起必须投递（held_keys 收敛；modifiers={modifiers:?}）"
            );
            assert!(
                key_delivered(KeyState::Up, modifiers, false),
                "普通抬起必须投递（modifiers={modifiers:?}）"
            );
        }
    }

    #[test]
    fn key_delivered_keeps_the_down_truth_table() {
        assert!(!key_delivered(KeyState::Down, Modifiers::empty(), true));
        assert!(!key_delivered(KeyState::Down, Modifiers::SHIFT, true));
        assert!(key_delivered(KeyState::Down, Modifiers::CONTROL, true));
        assert!(key_delivered(KeyState::Down, Modifiers::empty(), false));
    }
}

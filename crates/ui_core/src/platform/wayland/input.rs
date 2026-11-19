//! `wl_pointer.button` 的按钮码 → 引擎输入语义的纯映射（Linux 驱动层只做转发）。
//!
//! # 为什么放在这一层
//!
//! `wl_pointer.button` 的 `button` 参数直接转发 Linux **evdev** 的 `BTN_*` 码
//! （`linux/input-event-codes.h`：`BTN_LEFT = 0x110`、`BTN_RIGHT = 0x111`、
//! `BTN_MIDDLE = 0x112`）。旧实现把这组映射手写在 Linux-only 的
//! `platform/linux/surface.rs` 里，把 `BTN_RIGHT` / `BTN_MIDDLE` **写反了**：Linux 上右键
//! 产生 `PointerButton::Middle`、中键产生 `PointerButton::Secondary`，于是右键菜单/中键粘贴
//! 与 Windows（`WM_RBUTTONDOWN`）/ macOS（`rightMouseDown:`）的语义相反。而且映射位于
//! `cfg(target_os = "linux")` 的驱动文件里，开发机（Windows）上的 `cargo test` 一次也覆盖不到。
//!
//! 抽成这里的纯整数表后：三端语义由单测钉住，`surface.rs` 只保留协议事件转发（与
//! [`mod@crate::platform::wayland::cursor`] / [`mod@crate::platform::wayland::text_input`] 的
//! 分层一致）。
//!
//! 编译条件与同目录其它纯逻辑模块相同：Linux 构建 + 测试构建（`cfg(test)` 让 Windows 也能测）。

use crate::event::PointerButton;

/// `wl_pointer.button` 的 evdev 码 → 引擎 [`PointerButton`]。
///
/// 对照关系（evdev 与 Win32 / AppKit 的同一物理按键）：
///
/// | evdev | 值 | 引擎 | Windows | macOS |
/// |-------|----|------|---------|-------|
/// | `BTN_LEFT` | `0x110` | [`PointerButton::Primary`] | `WM_LBUTTONDOWN` | `mouseDown:` |
/// | `BTN_RIGHT` | `0x111` | [`PointerButton::Secondary`] | `WM_RBUTTONDOWN` | `rightMouseDown:` |
/// | `BTN_MIDDLE` | `0x112` | [`PointerButton::Middle`] | `WM_MBUTTONDOWN` | `otherMouseDown:` |
/// | `BTN_SIDE` | `0x113` | [`PointerButton::X1`] | `XBUTTON1` | `otherMouseDown:` button 1 |
/// | `BTN_EXTRA` | `0x114` | [`PointerButton::X2`] | `XBUTTON2` | `otherMouseDown:` button 2 |
///
/// 未映射的按钮返回 `None`：设备还会上报 `BTN_0`…`BTN_9`、`BTN_FORWARD` / `BTN_BACK`、
/// `BTN_TOOL_*`（绘图板工具）等，引擎没有对应语义，**不做猜测**（与 Windows 泵丢弃未知
/// 虚拟键的做法一致；猜成某个鼠标键会让「笔尖接触」变成一次点击）。
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

#[cfg(test)]
mod tests {
    use super::pointer_button;
    use crate::event::PointerButton;

    /// B7 全表：evdev `BTN_*` → 三端一致的 `PointerButton`。
    #[test]
    fn the_pointer_button_table_matches_evdev() {
        assert_eq!(pointer_button(0x110), Some(PointerButton::Primary));
        assert_eq!(pointer_button(0x111), Some(PointerButton::Secondary));
        assert_eq!(pointer_button(0x112), Some(PointerButton::Middle));
        assert_eq!(pointer_button(0x113), Some(PointerButton::X1));
        assert_eq!(pointer_button(0x114), Some(PointerButton::X2));
    }

    /// B7 回归：`BTN_RIGHT` 必须是 Secondary、`BTN_MIDDLE` 必须是 Middle。
    ///
    /// 旧实现（`surface.rs` 的 `WindowSurface::pointer_button`）写成 `0x111 → Middle`、
    /// `0x112 → Secondary`，用户右键得到中键语义（右键菜单从中键触发、中键粘贴落到右键），
    /// 与 Windows / macOS 相反。这一条单独断言给变异检查用：把两个值换回去这里立刻红。
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

    /// X1 / X2 保序：evdev `BTN_SIDE` → `X1`、`BTN_EXTRA` → `X2`，与 Windows
    /// `XBUTTON1`/`XBUTTON2`、macOS `otherMouseDown:` 的 1/2 编号同序（反了会让「后退」
    /// 触发「前进」）。
    #[test]
    fn side_buttons_keep_their_order() {
        assert_eq!(pointer_button(0x113), Some(PointerButton::X1));
        assert_eq!(pointer_button(0x114), Some(PointerButton::X2));
        assert_ne!(pointer_button(0x113), pointer_button(0x114));
    }

    /// 未映射的按钮不产生事件：设备专用码（`BTN_0`、`BTN_TOOL_PEN`、…）不得被猜成鼠标键。
    #[test]
    fn unmapped_buttons_are_dropped() {
        // 0x100 起是 BTN_MISC（设备专用），0x120 起是 BTN_JOYSTICK；鼠标键之后的下一个码
        // 是 0x115（BTN_FORWARD，本引擎没有「前进」语义）。
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
}

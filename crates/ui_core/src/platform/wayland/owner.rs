//! 窗口层级（X1）在 Wayland 上的**降级语义**：owner / 模态 / 置顶的解析与原因。
//!
//! # 协议事实（为什么只有降级）
//!
//! - **owner**：xdg-shell 没有「owner」概念，只有 `xdg_toplevel.set_parent` 的 *transient for*
//!   提示（协议原文：*"Set the \"parent\" of this surface. This surface should be stacked above
//!   the parent surface and all other ancestor surfaces ... so that the parent is raised when the
//!   dialog is raised."*）。compositor **可以忽略**它，协议也无法表达「随 owner 最小化 /
//!   销毁」⇒ Linux 端 owner 的落地只有一条 `set_parent` 请求，不保证 z 序或生命周期联动
//!   （公共 `WindowOptions::owner` 文档已写明该降级）。
//! - **模态**：协议没有模态；`xdg_toplevel` 的请求表里**没有** `set_modal`（不要臆造）。模态
//!   语义完全由 `App` 层输入拦截承担（与 macOS 同一套），本端只提供 owner 的 parent 提示。
//! - **置顶**：xdg-shell 没有置顶请求；wlr-layer-shell / KDE plasma-shell 都不是 xdg-shell
//!   标准能力 ⇒ 明确**降级为无操作**，应用不得假设成立。
//!
//! # 为什么放在这一层
//!
//! 「`WindowOptions::owner` → 该不该发 `set_parent`」是一个纯判定（未声明 / 已存在 / 不存在或
//! 已关闭），协议调用只有一句；判定抽在这里由 Windows 开发机的单测钉住（与 B7 按钮码、B8 滚轮
//! 累积、X2 拖动区的分层一致）。

use crate::window::WindowId;

/// `WindowOptions::owner` 在当前窗口集合下的落地决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerResolution {
    /// `owner == None`：普通顶层窗口（协议默认无父，不发 `set_parent`）。
    Unparented,
    /// owner 已存在：参数是它在 `WaylandState::windows` 里的下标，调用方对它发
    /// `xdg_toplevel.set_parent(Some(owner_toplevel))`（transient 提示）。
    Existing(usize),
    /// owner 已声明但当前不存在（尚未创建 / 已关闭 / 陈旧 id）：**降级不设**并写诊断，
    /// 不 panic（公共契约：owner 必须已存在；这里不假装成功，也不把请求发到错误窗口）。
    Missing(WindowId),
}

/// 解析 `WindowOptions::owner`；`lookup` 是「owner id → 原生窗口下标」的平台查询。
///
/// 调用时机是**新窗口 push 进 `WaylandState::windows` 之前**（`add_window` 里），因此 `lookup`
/// 看不到新窗口自己：`owner == 新窗口 id` 会落到 [`OwnerResolution::Missing`] —— 自指也是不可
/// 满足的 parent（协议：parent 不能是 child 自己，否则 `invalid_parent`）。
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

    /// 平台侧当前已建原生窗口的 id 表（顺序 = `WaylandState::windows` 的下标）。
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

    /// `owner == None`（默认）：普通顶层窗口，不发 `set_parent`。
    #[test]
    fn no_owner_is_unparented() {
        assert_eq!(
            resolve_owner(None, lookup),
            OwnerResolution::Unparented,
            "未声明 owner 不得设 parent"
        );
    }

    /// owner 已存在：解析到它在窗口表里的**下标**（不是 id），供调用方取父 toplevel。
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

    /// owner 不存在（尚未创建 / 已关闭 / 陈旧 id）：降级为 `Missing` ⇒ 调用方不设 parent 并写
    /// 诊断 —— 不得回落成「随便绑一个窗口」。
    #[test]
    fn a_missing_owner_degrades_to_missing() {
        for stale in [4, 99, u32::MAX] {
            assert_eq!(
                resolve_owner(Some(WindowId::for_test(stale)), lookup),
                OwnerResolution::Missing(WindowId::for_test(stale)),
                "不存在的 owner {stale} 必须降级为 Missing"
            );
        }
        // 显式断言「不得碰巧绑到下标 0」：旧实现若把失败当成成功，会把 parent 设成第一个窗口。
        assert_ne!(
            resolve_owner(Some(WindowId::for_test(4)), lookup),
            OwnerResolution::Existing(0)
        );
    }

    /// 新窗口自己还没 push 进窗口表（自指）：`lookup` 查不到 ⇒ `Missing`（不会把 parent 设成
    /// 自己，那会触发协议 `invalid_parent`）。
    #[test]
    fn a_self_reference_is_missing_before_the_window_is_pushed() {
        let new_window = WindowId::for_test(7);
        assert_eq!(
            resolve_owner(Some(new_window), lookup),
            OwnerResolution::Missing(new_window)
        );
    }

    /// 解析的是**下标**而非 id：窗口表顺序与 id 顺序不一致时，结果仍是在表里的位置。
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

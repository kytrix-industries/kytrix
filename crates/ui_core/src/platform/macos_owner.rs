//! macOS 窗口层级的**平台无关纯逻辑**（X1 原生部分）：`WindowOptions.owner` /
//! `modal` / `always_on_top` → AppKit `NSWindowLevel` 的选择与 owner 关系的解析。
//!
//! # 为什么在 `platform/` 根而不是 `platform/macos/` 下
//!
//! `platform::macos` 整模块只在 macOS 目标编译（objc2 依赖也是 macOS-only）；这里的映射
//! 是纯数据 + 纯函数，必须能在开发机（Windows）单测 —— 与 [`crate::platform::macos_placement`]、
//! [`crate::platform::macos_close`] 同一模式。真机侧只把枚举翻成 AppKit 常量、调
//! `NSWindow.setLevel:` 与 `addChildWindow:ordered:`。
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

#[cfg(test)]
mod tests {
    use super::{OwnerResolution, WindowId, WindowLevel, resolve_owner, window_level};

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
}

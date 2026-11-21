//! 内置 Debug HUD 的安装、采样与提层（`hud` feature）。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use super::App;
#[cfg(feature = "hud")]
use crate::debug_hud;
#[cfg(feature = "hud")]
use crate::debug_hud::DebugHudState;
use crate::{error::EngineError, window::WindowId};

/// 显示或隐藏内置 Debug HUD（P17；需要 `hud` feature）。
///
/// HUD 是窗口 root 末尾的普通子树：右上角浮动、不参与文档流、命中穿透、a11y 隐藏。
/// 它是**纯观察者**（P-C）：不请求 damage、不参与 `needs_continuous_frames`、没有任何定时器。
/// 文本只在应用自己出帧时按 500 ms 采样窗口刷新；静止窗口不产生帧，保持上一次文本。
///
/// **激活只请求一帧**（画面板，不采样、不补帧）：初始文本是 10 行缺省 `--` + 尺寸行占位；
/// 真实渲染目标（尺寸 / DPI / 后端）由帧入口的**按帧刷新**路径写入，安装帧就覆盖占位，窗口
/// resize 后的下一个应用帧即更新。因此**首次采样之前**静止窗口显示「10 行 `--` + 真实尺寸行」；
/// 有过应用帧采样之后再静止会保留上一次文本（HUD 不产生帧，也就没有新样本）；持续出帧 ≥500 ms
/// 后才会填入真实指标。关闭 HUD 同样只请求一帧擦像素。
/// 应用应在建完静态场景后开启 HUD；之后再追加的 root 子节点可能盖在它上面。
///
/// # Errors
/// `EngineError::UnknownWindow`：窗口不存在。建树失败时返回底层 `StaleNode` 等错误，
/// 且不会留下半棵 HUD 子树。
/// 消费者：`App::set_debug_hud`。
#[cfg(feature = "hud")]
pub(crate) fn set_debug_hud(
    app: &mut App,
    window: WindowId,
    enabled: bool,
) -> Result<(), EngineError> {
    if !app.windows.contains_key(&window) {
        return Err(EngineError::UnknownWindow { window });
    }
    let mut changed = false;
    if enabled {
        if let Some(root) = app.debug_hud.get(&window).map(DebugHudState::root) {
            // 外部若删掉了 HUD 子树，map 里的句柄会陈旧；重新安装而不是假装成功。
            let valid = app
                .windows
                .get(&window)
                .is_some_and(|target| target.tree().get(root).is_ok());
            if valid {
                return Ok(());
            }
            app.debug_hud.remove(&window);
        }
        let Some(target) = app.windows.get_mut(&window) else {
            return Err(EngineError::UnknownWindow { window });
        };
        let state = debug_hud::install(target, app.clock)?;
        app.debug_hud.insert(window, state);
        changed = true;
    } else if let Some(state) = app.debug_hud.remove(&window) {
        let _ = app.remove_node(window, state.root());
        changed = true;
    }
    if changed {
        app.request_frame();
    }
    Ok(())
}

/// 该窗口是否安装了 Debug HUD（未知窗口返回 `false`）。
/// 消费者：`App::debug_hud_enabled`。
#[cfg(feature = "hud")]
#[must_use]
pub(crate) fn debug_hud_enabled(app: &App, window: WindowId) -> bool {
    let Some(state) = app.debug_hud.get(&window) else {
        return false;
    };
    app.windows
        .get(&window)
        .is_some_and(|target| target.tree().get(state.root()).is_ok())
}

/// 帧入口的采样：把到期的文本放进 `pending_text`，由同一帧的 `apply_debug_hud` 写入。
///
/// **不请求 damage**（P-C）：HUD 不产生帧，也不修改任何脏位；没有应用帧就没有这次调用。
/// 消费者：`App::prepare_debug_hud`。
#[cfg(feature = "hud")]
pub(crate) fn prepare_debug_hud(app: &mut App, window: WindowId) {
    let now = app.clock;
    let Some(state) = app.debug_hud.get_mut(&window) else {
        return;
    };
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    debug_hud::prepare_frame(state, target, now);
}

/// `refresh_styles` 之后、`refresh_text` 之前写入 HUD 文本。
/// 消费者：`App::apply_debug_hud`。
#[cfg(feature = "hud")]
pub(crate) fn apply_debug_hud(app: &mut App, window: WindowId) {
    let Some(state) = app.debug_hud.get_mut(&window) else {
        return;
    };
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    debug_hud::apply_frame(state, target);
}

/// 新 overlay 会 append 到 root 末尾；HUD 必须保持在最上层。
/// 消费者：`App::raise_debug_hud`。
#[cfg(feature = "hud")]
pub(crate) fn raise_debug_hud(app: &mut App, window: WindowId) {
    let Some(state) = app.debug_hud.get(&window) else {
        return;
    };
    let hud = state.root();
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    let tree = target.tree_mut();
    let root = tree.root();
    let _ = tree.append_child(root, hud);
}

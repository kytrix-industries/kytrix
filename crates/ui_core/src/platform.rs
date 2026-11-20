//! 平台层（crate 私有；应用层不可见，I9）。
//!
//! Windows / macOS / Linux 三份实现按 `#[cfg(target_os)]` 编译期选定恰好一份（I14）；
//! 本文件只做模块声明与 crate 内出口，**不定义任何 item**（lint `definition_in_module_root`）。
//!
//! `event` 是 **Windows 专用**的 `WM_*` 解码层（`PlatformEvent` / `translate`）；macOS / Linux 的
//! 平台层直接把原生事件翻成公共 `Event` 后调 `App::dispatch`
//!
//! 三份实现都必须提供：`run(app)`、`window::NativeWindow`、`dpi`、`ime`、`clipboard`、`a11y`。

pub(crate) mod dispatcher;
/// IME 的跨平台纯逻辑：组合串子句归一化（Windows 的 `GCS_*` 原始字节 + Linux
/// `preedit_string` 的字节光标）、提交去重（macOS 按键作用域）、`replacementRange` 换算（macOS）。
///
/// **Windows 也编译**：`GCS_COMPATTR` / `GCS_COMPCLAUSE` 的原始字节在
/// `platform/ime::preedit_clauses` 里压成 `ImeEvent::Preedit::clauses` 区间，这一段纯逻辑
/// 因此可以在开发机上单测（而不是只在真机 IME 下才能观察）。macOS / Linux 专用项用
/// 各自的 `cfg` 关掉，避免 Windows 构建出现死代码。Linux 的 `preedit_carets`（字节偏移 →
/// UTF-16 码元）同样与平台类型无关，在 Windows 上被单测覆盖。
pub(crate) mod ime;

/// 原生主菜单的**平台无关模型**：菜单树、键盘等价物、AppKit 标准 selector、以及
/// 「selector → 引擎按键」的映射（R09-1）。
///
/// 只在 macOS 构建与测试构建里编译：只有 AppKit 有系统菜单栏承载这套 selector，而模型本身是
/// 纯数据 + 纯函数，开发机（Windows）的 `cargo test` 能覆盖它。
#[cfg(any(target_os = "macos", test))]
pub(crate) mod menu;

/// macOS 窗口摆放的**平台无关纯逻辑**（B3）：引擎逻辑坐标 ⇄ AppKit 全局屏幕坐标、
/// `NSScreen.visibleFrame` 工作区 clamp、内容 min/max 钳制、`position = None` 时的级联
/// 默认位置。
///
/// 与 [`menu`] 同一模式，只在 macOS 构建与测试构建里编译：`platform::macos` 整个模块是
/// macOS-only（objc2 依赖也是），而这里全是纯数据 / 纯函数，必须在开发机（Windows）单测。
#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_placement;

/// macOS 关闭语义的**平台无关状态机**（B5）：`performClose:` / `windowShouldClose:` 的用户
/// 手势（可否决）与 `windowWillClose:` 的外部关闭（不可否决、终态）必须严格区分，泵每轮
/// 据桥状态决定「投递事件 + 裁决 + 关窗」「只做关闭记账」还是「保留窗口」。
///
/// 与 [`macos_placement`] 同一模式：`platform::macos` 整模块 macOS-only，而这里不依赖
/// AppKit 类型，状态迁移必须在开发机（Windows）单测。
#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_close;

/// macOS 窗口层级的**平台无关纯逻辑**（X1 原生部分）：`always_on_top` / `modal` → AppKit
/// `NSWindowLevel` 的选择（模态 > 置顶 > 普通）、`WindowOptions.owner` →
/// `addChildWindow:ordered:` 关系的解析（含「平台侧找不到 owner 原生窗口」的降级判据），以及
/// 模态期对 owner 的**原生 chrome 拦截**（`OwnerChrome` 快照/恢复、同 owner 多模态的派生
/// 恢复判据、泵级 key 预扫描 `reclaim_modal_keys`，RB-07/RB-13-mac/RBM-01）。
///
/// 与 [`macos_close`] 同一模式：`platform::macos` 整模块 macOS-only，这里不依赖 AppKit
/// 类型，映射与优先级必须在开发机（Windows）单测。
#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_owner;

/// macOS 自绘标题栏**拖动区**的纯逻辑（X2）：区域是逻辑窗口坐标（客户区左上原点、y 向下），
/// `GuiView` 是 flipped，因此只需半开区间矩形包含判定；命中后真机侧把拖动交给 WindowServer
/// （`performWindowDragWithEvent:`）而不投递 `PointerDown`，并用 [`WindowDragState`] 记录
/// 「命中后的主键 Up/Move 没有配对 Down」的吞并状态（RB-08）。
///
/// 与 [`macos_owner`] 同一模式：`platform::macos` 整模块 macOS-only，命中判定与配对状态机必须
/// 在开发机（Windows）单测（区域内/外、边界点、空/负尺寸区域、多区域顺序、y 向下、拖动期
/// Up/Dragged 吞并、hover 结束拖动）。
#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_drag;

/// macOS 交通灯安全区的**平台无关纯逻辑**（E6 §8.6.5）：三个 `standardWindowButton:` 在内容区
/// 坐标系里的 frame → 应用需要留白的外边界（`App::traffic_light_inset` 的数据来源）。
///
/// 与 [`macos_placement`] 同一模式：`platform::macos` 整模块 macOS-only，而「安全区多大」是纯
/// 几何，必须在开发机（Windows）单测（空集 / 部分按钮缺失 / 零矩形 / 半像素）。
#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_chrome;

/// Wayland 协议层的纯逻辑（`wl_data_source` 生命周期、IME 焦点路由、光标形状/主题、
/// `xdg_toplevel` 状态机）。
///
/// 只在 Linux 构建与测试构建里编译：`wayland-client` 只在 Linux 目标是依赖，而这几条判据
/// 必须在开发机（Windows）的 `cargo test` 里跑得到（见模块文档）。
#[cfg(any(target_os = "linux", test))]
pub(crate) mod wayland;

#[cfg(windows)]
pub(crate) mod event;
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux as native;
#[cfg(target_os = "macos")]
pub(crate) use macos as native;
#[cfg(windows)]
pub(crate) use windows as native;

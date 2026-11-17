//! 窗口：最小尺寸、CSD 装饰度量，以及 **`wl_region`（输入区 / 不透明区）的已知取舍**（R10-11）。
//!
//! Wayland 没有「窗口对象」：状态在 `WaylandState`（surface/toplevel）里，本模块只放
//! 与协议无关的窗口常量与最小尺寸设置，避免它散落到事件翻译层。
//!
//! 最小尺寸是 **window geometry 的逻辑像素**，与 `scale` 无关：xdg-shell 的
//! `xdg_toplevel.set_min_size` 参数是 window geometry 坐标，不是 buffer 像素。本仓用
//! `wp_viewport.set_destination(ceil(logical))` 让 surface 尺寸等于逻辑尺寸（S-144），所以
//! 320×200 直接就是逻辑值；因此它只需在建窗时（`wl_surface` 首个 commit 之前）设置一次
//! （V-122 ⑦）。
//!
//! **阴影边距**（R10-10）：引擎目前不画投影，[`SHADOW_MARGIN`] 是 0，于是 window geometry
//! 就是整个 surface，`set_min_size(320, 200)` 正好约束客户区。将来加投影时只需把边距改成非 0：
//! `xdg_surface.set_window_geometry` 会据此收缩（阴影落在几何之外），`set_min_size` 会据此
//! 把边距补回去（否则窗口能被缩到内容不可用）。两处都走
//! [`crate::platform::wayland::toplevel`] 的纯函数，语义不会分叉。
//!
//! 没有 viewporter 时走整数回退（buffer = `round(逻辑 × N)`、`wl_surface.set_buffer_scale(N)`，
//! 见 `surface.rs`）：Wayland 的逻辑尺寸已在平台层取整，buffer 严格是 N 的整数倍（否则
//! `wl_surface.attach` 报 `invalid_size`），surface 的逻辑尺寸因此等于 `buffer / N`，
//! 本方法写入的 320×200 仍是期望的最小客户区。
//!
//! # `wl_region`：输入区 / 不透明区（R10-11 / R10-33）的已知取舍
//!
//! AR3 §3.8 把 R10-11 判为「未做」。这里给出**结论、依据与触发条件**，而不是一句「无阴影
//! 可省」：现状是「引擎侧不创建 `wl_region`」这一**有意取舍**，一旦下列触发条件成立就必须补，
//! 而不是等用户报「点击穿透 / 窗口背后内容漏出来」。
//!
//! ## 协议事实
//!
//! - `wl_surface.set_input_region(region)`：*"Sets the input region for the surface. The input
//!   region is the region of the surface that can receive pointer and touch events. … **Default
//!   is the whole surface.** Setting an empty region will mean that the surface will be in an
//!   'input-transparent' state."* 未设置 ⇒ **整个** surface 都接收指针事件。
//! - `wl_surface.set_opaque_region(region)`：*"Sets the region of the surface that is opaque. …
//!   the compositor may use this information to optimize …"* 未设置 ⇒ compositor 必须假定
//!   surface 可能半透明（每帧都与背后内容混合）。
//! - 两者都是**双缓冲状态**：与 `attach`/`commit` 一起生效（本仓由 GPU 线程在 present 时
//!   commit，因此平台层只能在 present 之前写）。
//!
//! ## 为什么现在两者都等于「整个 surface」
//!
//! 三条事实（都可以在代码里逐条核对）：
//!
//! 1. **surface 里没有「视觉上不存在」的像素**：[`SHADOW_MARGIN`] = 0 ⇒
//!    [`crate::platform::wayland::toplevel::window_geometry`] 返回的矩形就是整个 surface
//!    （单测 `window_geometry_without_shadow_covers_the_whole_surface` 钉住这一点）。引擎
//!    也不画圆角 / 边框，buffer 是整面不透明的清屏 + 内容。
//! 2. **引擎不自绘 CSD 标题栏**：`crate::window::WindowState::client_decoration` 是「该不该画
//!    标题栏」的判据（compositor 回 `server_side` 时为 `false`），但它目前**没有消费者** ——
//!    `Window::state()` 只被存取、没有任何绘制路径读它（全仓 grep：写入点只有
//!    `platform/linux/surface.rs` 的 `window_state`）。因此不存在「SSD 时仍预留了一条标题栏
//!    条带、而那条带不该吃点击」的情形。
//! 3. **SSD 的标题栏在 surface 之外**：`zxdg_toplevel_decoration_v1` 的语义是 compositor 在
//!    toplevel 周围画装饰（frame），客户端 surface 只是内容区。所以 `server_side` 时也没有
//!    「surface 内但不属于窗口」的矩形需要从 input region 里挖掉。
//!
//! 结论：**默认行为（不调用 `set_input_region`）恰好就是正确的输入区**；显式设一个与 surface
//! 等大的 region 是纯 no-op，只会多两次协议往返 + 一段没有消费者的代码（AGENTS §1.2 禁止预防性
//! 抽象、§4 禁止无用代码）。`set_opaque_region` 同理，但**风险更高**（见下）所以更不能顺手设。
//!
//! ## 触发条件（出现任一条就必须实现）
//!
//! | 触发条件 | 不实现的后果 | 必须做的事 |
//! |---|---|---|
//! | [`SHADOW_MARGIN`] > 0（开始画投影） | 阴影带落在 window geometry 之外却仍吃指针事件：窗口四边出现一圈「点下去什么都不会发生、但会抢走点击」的死区 | `set_input_region(window_geometry)`（挖掉四边阴影带）；`set_opaque_region` 同样收缩到 geometry |
//! | 引入圆角 / 半透明边框（buffer 出现 alpha < 1 的像素） | compositor 的遮挡剔除依据错误 ⇒ 看到窗口背后不该看到的内容「漏出来」（视觉错误，不是性能问题） | `set_opaque_region(整窗 − 圆角)`,并在同一份矩形基础上决定 input region |
//! | 应用层开始实现自绘标题栏（`Window::state()` 出现消费者） | 双标题栏（compositor 的 + 自绘的） | 引擎侧仍然无需 region（CSD 条带属于窗口）；应用必须按 `WindowState::client_decoration` 决定画不画 |
//! | 需要输入穿透（tooltip / 拖拽预览 / 悬浮层） | 悬浮层的透明部分挡住下层窗口的点击 | `set_input_region(空 region)`（协议原文的 input-transparent） |
//!
//! `set_opaque_region` 单独再强调一次：**本仓目前没有「整窗必不透明」的强制契约**（
//! `App` 的背景色来自窗口 options、清屏 alpha 在 `render/**`，都不在本模块可见范围内）。
//! 把一个并不成立的不透明假设告诉 compositor，代价是**视觉错误**；而省下的合成开销在
//! 本仓从未实测。所以「等真的出现半透明/圆角，连同 alpha 契约一起做」比「先设整窗不透明」
//! 安全 —— 这是本条取舍的核心，也是它与「无阴影可省」的区别。
//!
//! ## 怎么验证本结论仍然成立
//!
//! - 静态：`SHADOW_MARGIN == 0` 且没有绘制圆角/标题栏的代码路径（`Window::state()` 无消费者）。
//! - 真机：`WAYLAND_DEBUG=1` 观察本进程**没有**发 `wl_surface.set_input_region` /
//!   `set_opaque_region`；点击窗口最上/最下行、四角仍然命中窗口（说明「整个 surface 都是
//!   内容」这一前提没被破坏）。

use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;

use crate::platform::wayland::toplevel;

/// 最小客户区（逻辑像素；与 Win32 / macOS 同源）。
pub(crate) const MIN_SIZE_LOGICAL: (f32, f32) = (320.0, 200.0);

/// CSD 阴影 / 边框在 window geometry **之外**占的逻辑像素（每边）。
///
/// 0 = 不自绘阴影（当前实现）：window geometry 等于整个 surface，因此引擎侧不需要
/// `wl_surface.set_input_region`（见模块文档「`wl_region` 的已知取舍」）。改成非 0 的那一刻
/// 必须同时补输入区，否则窗口四边会出现吃点击的死区。
pub(crate) const SHADOW_MARGIN: f32 = 0.0;

// 编译期闸门：`SHADOW_MARGIN == 0` 是「引擎侧不需要 `wl_region`」结论的前提。
// 改这个常量会**直接构建失败**，强制实现者先读模块文档、把输入区（以及不透明区）一起补上，
// 而不是改完常量让窗口四边变成吃点击的死区。（`f32::to_bits` 是 const fn，浮点比较不是。）
const _: () = assert!(
    SHADOW_MARGIN.to_bits() == 0,
    "SHADOW_MARGIN 改成非 0 时必须同时实现 wl_surface.set_input_region（见本模块文档「wl_region 的已知取舍」）"
);

/// 把最小尺寸交给 `xdg_toplevel`：window geometry 的逻辑像素（含阴影边距）。
///
/// 每个窗口只在建窗时（`WaylandState::add_window` 的首个 commit 之前）调用一次：DPI 变化后
/// 重发会把最小客户区按 `scale` 放大（V-122 ⑦）。
pub(crate) fn apply_min_size(toplevel: &XdgToplevel) {
    let (width, height) = toplevel::min_size_with_shadow(MIN_SIZE_LOGICAL, SHADOW_MARGIN);
    toplevel.set_min_size(width, height);
}

//! Wayland 协议层的**协议无关纯逻辑**（模块根只做声明）。
//!
//! 为什么单独一层：本仓的 Linux 平台代码（`platform/linux/**`）只在 `target_os = "linux"` 下
//! 编译，而 `wayland-client` 也只在 Linux 目标下是依赖 —— 于是「Wayland 状态机写错」这类缺陷
//! 在开发机（Windows）的 `cargo test` 里**一次也跑不到**。R10 里被判定为 P0/P1 的几条恰恰全都
//! 是状态机而非 API 误用：
//!
//! - R10-1 `wl_data_source` 复用且不销毁 ⇒ 第二次复制后粘贴为空；
//! - R10-2/R10-6/R10-13 IME 的焦点路由、无 IM 守护进程时无回退、`done` 按 `active` 路由；
//! - R10-3 完全没有光标实现（形状映射表 + Xcursor 主题解析都在这里）；
//! - R10-7 不可见窗口的活性兜底退化成永久 5 Hz 渲染（帧门禁状态机 `frame_gate` 在这里，
//!   它只认 `wl_callback` / 可见性，不认时间）。
//!
//! 因此这些判断被抽成「输入是协议里的整数码 / 纯 Rust 状态，输出是可直接下发的决定」的纯
//! 函数与纯状态机，由 [`mod@self`] 的单测覆盖（开发机跑得到）。协议对象（`wl_data_source`、
//! `zwp_text_input_v3`、`wl_pointer`…）只出现在 `platform/linux/**` 的薄驱动层。
//!
//! P1 追加的纯逻辑同理：
//!
//! - **X2 拖动区**（[`drag`]）：自绘标题栏的主键按下命中拖动区 ⇒ 平台层发 `xdg_toplevel.move`
//!   交给 compositor 驱动移动、不投递 `PointerDown`（判定在 Windows 开发机上可单测）；
//! - **X1 窗口层级**（[`owner`]）：`WindowOptions::owner` → `xdg_toplevel.set_parent` 的
//!   transient 提示解析（Wayland 没有 owner / 模态 / 置顶的跨合成器协议，降级语义在模块里
//!   写清：模态由 `App` 层输入拦截承担、置顶无操作）。
//! - **A4 呈现证据**（[`presentation`]）：`wp_presentation` 的纯判据 —— 呈现时钟域白名单
//!   （`CLOCK_REALTIME` / `CLOCK_MONOTONIC`）与 presented 证据的单调判据；协议对象、GPU 线程
//!   的 feedback 请求与时钟换算在 `platform/linux/presentation.rs`。
//!
//! 编译条件与 `platform/ime` 同款：Linux 构建 + 测试构建（`cfg(test)` 让 Windows 也能测）。

pub(crate) mod close;
pub(crate) mod configure;
pub(crate) mod cursor;
pub(crate) mod drag;
pub(crate) mod frame_gate;
pub(crate) mod ime;
pub(crate) mod input;
pub(crate) mod owner;
pub(crate) mod presentation;
pub(crate) mod scroll;
pub(crate) mod selection;
pub(crate) mod text_input;
pub(crate) mod toplevel;

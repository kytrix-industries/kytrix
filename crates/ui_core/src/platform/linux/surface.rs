//! Wayland surface / 协议状态机（`design/phase-12-macos-linux.md` §8.2 / §8.8）。
//!
//! 本文件是 **唯一的协议事件翻译点**：所有 `wl_*` / `xdg_*` / `wp_*` 事件在这里变成公共
//! `Event`（与 Win32 的 `translate` 同层职责），泵在主循环里 drain 后交给 `App::dispatch`。
//!
//! 约束（D25 / I16）：只走 Wayland；缺必需 global 一律 `EngineError::Wayland`，**不回退 X11**。
//!
//! 协议正确性的关键点（R10 整改后）：
//!
//! - **建窗顺序**（R10-4）：`wl_surface` → `xdg_surface` → `xdg_toplevel` → 静态属性
//!   （app_id/title/min_size/default_size）→ `zxdg_toplevel_decoration_v1`（+`set_mode`）→
//!   fractional scale → viewport → **最后**才 `surface.commit()`。xdg-decoration **v1** 明确
//!   规定「toplevel 已经 attach/commit 过 buffer 之后再创建 decoration 是 client error」，
//!   而 `commit()` 正是触发首个 configure 的那一次。
//! - **必须遵从装饰 configure**（R10-4）：compositor 回 `server_side` 时引擎不得再自绘标题栏
//!   （否则双标题栏）；模式写进 `crate::window::Window::state()`（应用每帧可读）。
//! - **window geometry**（R10-10）：每次尺寸/装饰变化后 `xdg_surface.set_window_geometry`
//!   （扣除阴影边距，当前无阴影 ⇒ 整个 surface），compositor 用它做放置/动画/阴影。
//! - **不可见不请求帧**（R10-7）：`wl_surface.enter/leave` 与 `Suspended` 共同决定
//!   [`WaylandState::is_visible`]；不可见的窗口既不请求 `wl_surface.frame`，也不启动活性兜底。
//! - **IME 按 surface 路由**（R10-6/R10-13）：`zwp_text_input_v3.enter/leave` 是权威焦点，
//!   `done` 的批次只投给那个 surface 的窗口；无输入法守护进程时降级回键盘文本通路（R10-2，
//!   判据在 [`crate::platform::wayland::text_input`]）。
//! - **seat 能力撤销要 `release()`**（R10-8）：直接丢代理会让服务端继续往已释放的 id 发事件，
//!   那是协议错误（断连）。
//!
//! 缩放（V-110 / V-122 ④）：
//! - **fractional**：`wp_viewporter` 与 `wp_fractional_scale_manager_v1` **同时**存在时才启用
//!   （`uses_viewport`，fractional 对象与 viewport 对象成对）。buffer 按
//!   `round(逻辑 × preferred_scale/120)` 出图，`wp_viewport.set_destination(ceil(逻辑))` 由 GPU
//!   线程在 present 前写入，`wl_surface` 的 buffer scale 保持默认 1（由 destination 负责缩放）。
//! - **整数回退**（缺 viewporter 或 fractional manager）：**不创建** fractional / viewport 对象。
//!   整数 scale `N` 优先取 `wl_surface.preferred_buffer_scale`（surface v6），否则取已 enter 的
//!   `wl_output.scale` 最大值；**首次 enter 之前**退到所有已绑定 output 的最大 scale（单屏 HiDPI
//!   首帧即正确），收到过 enter 之后若 entered 列表为空（最小化 / 切工作区）保留上一个值，避免
//!   虚假的 DPI 抖动；仍然没有才是 1。Wayland 的逻辑尺寸在平台层取整，buffer 按
//!   `round(逻辑 × N) = 逻辑 × N` 出图、严格是 N 的整数倍（否则 `wl_surface.attach` 报
//!   `invalid_size`），`buffer_scale = N` 由 GPU 线程与 attach/commit 同一次生效。surface 的
//!   逻辑尺寸因此等于 `buffer / scale`，不会再出现「buffer 按分数放大、buffer_scale=1」把
//!   surface 放大 1.25× 的实测缺口。
//!
//! `wl_output` 是热插拔的多实例 global：启动时绑定全部现存实例（不能用 `GlobalList::bind`，
//! 它只绑第一个），`wl_registry.global` 到达时动态绑定、`global_remove` 时解绑并从已 enter
//! 列表移除；scale 变化会 push `Event::DpiChanged`。

use std::time::{Duration, Instant};

use smol_str::SmolStr;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::ObjectId,
    event_created_child,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_data_source, wl_keyboard, wl_keyboard as wl_kb, wl_output, wl_pointer,
        wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::{
    wp::{
        cursor_shape::v1::client::{
            wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
            wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
        },
        fractional_scale::v1::client::{
            wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, wp_fractional_scale_v1,
        },
        text_input::zv3::client::{
            zwp_text_input_manager_v3::ZwpTextInputManagerV3,
            zwp_text_input_v3::{self, ZwpTextInputV3},
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::{
        decoration::zv1::client::{
            zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, zxdg_toplevel_decoration_v1,
        },
        shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
    },
};
use xkbcommon::xkb;

use crate::{
    app::App,
    error::{EngineError, WaylandErrorKind},
    event::{
        Event, ImeEvent, InputEvent, KeyCode, KeyState, Modifiers, PointerButton, ScrollPhase,
        ScrollUnit, should_emit_text,
    },
    math::{Point, Rect, Scale, Size},
    paint::style::Cursor,
    platform::{
        linux::{
            clipboard::Clipboard,
            cursor::CursorManager,
            dpi,
            frame_gate::{CallbackToken, FrameGate},
            ime::ImeState,
            window,
        },
        wayland::{
            text_input::{self, ContentTypeBits, TextInputRoute},
            toplevel::{
                self, DecorationMode, ToplevelStates, Visibility, WindowGeometry, WmCapabilities,
            },
        },
    },
};

/// 客户端自报的 app id（compositor 用来匹配桌面文件）。
const APP_ID: &str = "kytrix";

/// 一个已绑定的 `wl_output`（多显示器：整数回退取已 enter 的最大 scale）。
struct OutputEntry {
    output: wl_output::WlOutput,
    /// registry 的 global name（动态绑定 / `global_remove` 的匹配键）。
    name: u32,
    /// 最近一次 `wl_output.scale`（协议保证非零正数；本地再 clamp 到 ≥1）。
    scale: i32,
}

/// 整数回退的 scale 选择（纯函数，便于单测；V-110）。
///
/// 优先级：`preferred_buffer_scale` → 已 enter 的 output 最大 scale → **仅在尚未收到过 enter
/// 时**退到全部已绑定 output 的最大 scale → 上一个整数 scale（首次 enter 之后若列表为空，
/// 说明窗口暂时离开了所有 output：最小化 / 切工作区，保留旧值避免虚假 DPI 抖动）。全程 clamp ≥1。
fn choose_integer_scale(
    preferred: Option<i32>,
    entered_max: Option<i32>,
    bound_max: Option<i32>,
    previous: i32,
    entered_once: bool,
) -> i32 {
    if let Some(preferred) = preferred {
        return preferred.max(1);
    }
    if let Some(entered) = entered_max {
        return entered.max(1);
    }
    if !entered_once && let Some(bound) = bound_max {
        return bound.max(1);
    }
    previous.max(1)
}

/// 键盘重复参数（`wl_keyboard.repeat_info`）。
///
/// Wayland 与 X11/Win32/macOS 不同：**服务端不合成重复**，客户端必须自己按 `rate`/`delay`
/// 产生重复的 Key Down。缺了它长按 Backspace/方向键只生效一次（P1-2.11）。
#[derive(Clone, Copy, Debug)]
struct KeyRepeat {
    /// 重复速率（Hz）；`0` = 服务端要求关闭重复。
    rate: u32,
    /// 首次重复前的延迟（毫秒）。
    delay: u32,
}

impl Default for KeyRepeat {
    fn default() -> Self {
        // 服务端未发 `repeat_info` 时的保守默认（与 GTK 一致：25 Hz / 500 ms）。
        Self {
            rate: 25,
            delay: 500,
        }
    }
}

impl KeyRepeat {
    /// 重复间隔；`None` = 不重复（`rate == 0`）。
    fn interval(self) -> Option<std::time::Duration> {
        (self.rate > 0).then(|| std::time::Duration::from_micros(1_000_000 / u64::from(self.rate)))
    }
}

/// 一个处于按下状态的键（合成重复用）。
#[derive(Clone, Copy, Debug)]
struct HeldKey {
    /// evdev keycode（`wl_keyboard.key` 原值，未减 8）。
    key: u32,
    /// 下一次应当合成重复的时刻。
    next: std::time::Instant,
}

/// 一次 `wl_pointer.frame` 内累积的滚轮增量。
///
/// Wayland 的 `axis` 是连续值（surface-local 单位，触控板即像素、滚轮一格通常 10–15），
/// `axis_value120`（v8+）才是 1/120 行的高精度离散步长 —— 后者与 Windows 的
/// `WHEEL_DELTA = 120` 同量纲，可以直接除以 120 得到行数。旧实现把 `axis` 除以 120，
/// 于是滚轮慢约一个数量级（P1-2.11）。
#[derive(Clone, Copy, Debug, Default)]
struct ScrollAccum {
    /// `axis_value120` 累积（1/120 行）。
    discrete_x: i32,
    discrete_y: i32,
    /// 连续 `axis` 累积（surface-local 单位）。
    continuous_x: f32,
    continuous_y: f32,
}

impl ScrollAccum {
    fn is_empty(self) -> bool {
        self.discrete_x == 0
            && self.discrete_y == 0
            && self.continuous_x == 0.0
            && self.continuous_y == 0.0
    }

    /// 换算成「行」。
    ///
    /// 离散部分走 `value120 / 120`（精确）；连续部分按 `CONTINUOUS_AXIS_UNITS_PER_LINE`
    /// 折算 —— Wayland v5 起滚轮一格被规定为 10 个单位，取 10 让连续通路在滚轮上得到 1 行/格，
    /// 触控板则按手指位移成比例平滑滚动。
    fn lines(self) -> (f32, f32) {
        (
            self.discrete_x as f32 / 120.0 + self.continuous_x / CONTINUOUS_AXIS_UNITS_PER_LINE,
            self.discrete_y as f32 / 120.0 + self.continuous_y / CONTINUOUS_AXIS_UNITS_PER_LINE,
        )
    }

    /// 连续部分按**像素**（触控板 / `axis_source = finger` 的 1:1 跟随，R02-10）。
    ///
    /// 手指输入的 `axis` 值本来就是「手指移动量 × 加速度」，不做行换算才能与系统惯性对齐；
    /// 方便起见把同时到达的离散分量按一格 = 10 个单位折算进来（正常不会同时出现）。
    fn continuous_pixels(self) -> (f32, f32) {
        (
            self.continuous_x + self.discrete_x as f32 / 120.0 * CONTINUOUS_AXIS_UNITS_PER_LINE,
            self.continuous_y + self.discrete_y as f32 / 120.0 * CONTINUOUS_AXIS_UNITS_PER_LINE,
        )
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// 连续 `axis` 的单位→行换算（见 [`ScrollAccum::lines`]）。
const CONTINUOUS_AXIS_UNITS_PER_LINE: f32 = 10.0;

/// 一个原生窗口的 Wayland 侧状态（surface / toplevel / 几何 / 帧节流）。
///
/// **进程级 vs 每窗口**（P1-2.8 的分层）：`WaylandState` 持有连接、seat、键盘/指针、
/// xkb、IME、剪贴板、事件队列 —— 这些跨窗口共享；本结构只放「一个 surface 独有」的东西。
/// `WaylandState::windows` 是这些状态的集合，事件按对象（`wl_surface` / `xdg_toplevel` /
/// `wl_callback`）或焦点窗口路由到其中的某个下标。
///
/// 键盘/指针的事件属于 **seat**，不属于 surface：它们的路由依据是「哪个窗口当前有焦点 /
/// 指针进入」。
struct WindowSurface {
    /// 稳定 id（与 `App` 的 `WindowId` 对应）。
    id: crate::window::WindowId,
    surface: wl_surface::WlSurface,
    /// `xdg_surface`（首个 configure 的 ack 在 handler 里；析构顺序见 `Drop`）。
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    /// 应用声明的 IME caret 区域（surface 逻辑坐标；`None` = 无 hint）。
    ime_cursor_area: Option<Rect>,
    /// 最近一次实际下发给 `zwp_text_input_v3` 的区域（含 None 回退）。
    ime_cursor_applied: Option<Rect>,
    /// 最近一次下发的 `content_type`（`None` = 还没发过；变化时才重发，R10-12）。
    ime_content_applied: Option<ContentTypeBits>,
    /// 应用声明的文本输入语义（密码 / PIN 必须开 `sensitive_data`，R10-12）。
    ime_content: crate::window::ImeContentType,
    /// 本窗口的 IME 批次状态（`done` 时原子生效；**每窗口一份**，R10-6）。
    ime: ImeState,
    /// 分数缩放对象：按它路由 `PreferredScale`（该事件不告诉我们是哪个窗口）。
    ///
    /// 只有 `wp_viewporter` 也存在时才创建（否则走整数回退，V-110）。
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    /// 分数缩放的落地窗口（`set_destination` 让 compositor 做非整数缩放，D-5）。
    ///
    /// 建窗后移交给窗口渲染器（[`WaylandState::take_viewport`]）：destination 必须与 buffer 在
    /// 同一次 present 里生效，只有 GPU 线程持有它才能在 `vkQueuePresentKHR` 之前写入（D1 / D2）。
    viewport: Option<WpViewport>,
    /// 是否创建了 `wp_viewport`（= `wp_viewporter` 与 fractional manager **同时**存在）。
    ///
    /// `true` ⇒ fractional 路径：[`WaylandState::buffer_scale`] 恒 1（destination 负责缩放）；
    /// `false` ⇒ 整数回退：`buffer_scale = N`，buffer 是 `round(逻辑 × N)`（V-110）。
    /// `take_viewport` 之后仍保持 `true`。
    uses_viewport: bool,
    /// `zxdg_toplevel_decoration_v1`（compositor 支持 client-side 装饰时存在）。
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    /// compositor 最近一次选定的装饰模式（R10-4；`None` = 没收到/没有该扩展 ⇒ 自绘）。
    decoration_mode: Option<DecorationMode>,
    size: Size,
    scale: Scale,
    /// 本 surface 已 enter 的 output 对象 id（enter / leave 成对；整数回退取其中最大 scale）。
    surface_outputs: Vec<ObjectId>,
    /// `wl_surface.preferred_buffer_scale`（surface v6；收到时优先于 output scale）。
    preferred_buffer_scale: Option<i32>,
    /// `wp_fractional_scale_v1.preferred_scale`（120 分之几；只有 fractional 模式才会收到）。
    ///
    /// 未收到时回落到整数 scale（`preferred_buffer_scale` / 已 enter 的最大值 / 首次 enter 前
    /// 全部已绑定的最大值 / 上一个整数 scale）。
    fractional_scale_120: Option<u32>,
    /// 当前整数回退 scale（初始 1；每次 `recompute_scale` 推进）。
    ///
    /// 独立于 `scale` 保存：leave 清空 entered 列表后要保留上一个值，否则窗口最小化 /
    /// 切工作区时会从 1× 跳到大屏的 2×、恢复时再跳回，产生两次无意义的全量重排（V-110）。
    integer_scale: i32,
    /// 是否收到过至少一个 `wl_surface.enter`（一旦为 `true` 就保持）。
    ///
    /// enter 之前整数 scale 用「全部已绑定 output 的最大值」做保守估计；enter 之后若列表为空
    /// （最小化 / 切工作区）保留上一个值，不再回退到 bound 最大值。
    entered_once: bool,
    focused: bool,
    configured: bool,
    /// 最近一次 configure 的状态位（`Maximized` / `Fullscreen` / `Suspended` / `Tiled*`）。
    states: ToplevelStates,
    /// compositor 声明的窗口管理能力（xdg-shell v5；未收到 ⇒ 假定全部支持，R10-15）。
    wm_caps: WmCapabilities,
    /// compositor 建议的尺寸上限（`configure_bounds`，xdg-shell v4；`None` = 未收到）。
    bounds: Option<Size>,
    /// 最近一次下发的 window geometry（同值不重复发，R10-10）。
    geometry: Option<WindowGeometry>,
    /// 指针进入本窗口时应使用的光标（应用通过 `take_cursor_change` 设置，R10-3）。
    cursor: Cursor,
    close_requested: bool,
    /// 帧节流：上一次请求的 `wl_callback` 回来之前不再提交（Idle / 超时兜底见 `frame_gate`）。
    frame_gate: FrameGate,
    /// 最近一次 `wl_surface.frame` 的回调对象 id 与门禁令牌。
    ///
    /// 门禁的活性兜底会放弃旧 callback 再挂新的；回调事件只带自己这个对象，因此靠 id 区分
    /// 「当前请求的 callback」与「被兜底放弃、迟到的旧 callback」，令牌只对匹配的回调生效。
    frame_callback: Option<(ObjectId, CallbackToken)>,
}

impl Drop for WindowSurface {
    fn drop(&mut self) {
        // Wayland 的代理**不会**在 drop 时自动发析构请求（它只是句柄），窗口移除必须显式销毁
        // 服务端对象，否则 compositor 侧窗口会一直留在屏幕上。顺序按角色的创建逆序：
        // 装饰 / 分数缩放 / viewport 先于它们引用的 toplevel / surface，`xdg_toplevel` 必须
        // 先于 `xdg_surface`（协议要求 xdg_surface 在其 role 对象之后销毁），`wl_surface` 最后。
        //
        // viewport 在渲染器创建时已移交（`take_viewport`，`None`）；它由渲染器的 `Drop` 销毁。
        if let Some(decoration) = self.decoration.take() {
            decoration.destroy();
        }
        if let Some(fractional) = self.fractional.take() {
            fractional.destroy();
        }
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        self.toplevel.destroy();
        self.xdg_surface.destroy();
        self.surface.destroy();
    }
}

pub(crate) struct WaylandState {
    /// 全部窗口的 per-surface 状态（P1-2.8 Stage B）。
    ///
    /// 事件的归属由对象解析（`wl_surface` / `xdg_toplevel` / `wl_callback` → 窗口下标）；
    /// seat 的键盘/指针事件不属于任何 surface，按 [`Self::active`] 派发。
    windows: Vec<WindowSurface>,
    /// 当前**焦点窗口**在 [`Self::windows`] 里的下标。
    ///
    /// 键盘事件、IME、剪贴板属主都跟随它；`wl_keyboard.enter` 更新它。
    /// `remove_window` 会按移除位置修正；没有窗口时不会访问（泵在集合空之前退出）。
    active: usize,
    /// 指针当前进入的窗口下标（`wl_pointer.enter` / `leave` 维护）。
    ///
    /// 与 [`Self::active`]（键盘焦点）**分开**：鼠标在一个窗口、键盘焦点在另一个窗口是常态
    /// （悬浮预览、非聚焦窗口上的 hover）。指针事件按它派发。
    pointer_focus: Option<usize>,
    /// 待泵消费的平台事件，**带窗口 id**（P1-2.8 Stage C-2）。
    ///
    /// 事件循环按 id 派发到对应的 `Window`；seat 的键盘/指针事件归 `active` 窗口。
    events: Vec<(crate::window::WindowId, Event)>,
    // ── 持有协议对象、且在 `Drop` 里**发请求**的字段必须声明在 `connection` 之前 ──
    //
    // Rust 按声明顺序析构字段：`windows` / `cursor` / `clipboard` 的 `Drop` 都会 `destroy()`
    // 服务端对象（`wl_surface` / 光标 buffer / `wl_data_source`）。它们必须比 `Connection` 先
    // 析构 —— 否则请求写进已关闭的连接（wayland-client 会静默失败并记日志，服务端对象泄漏到
    // 连接关闭为止）。这一条与 I14 的「析构顺序」纪律同源。
    /// 光标下发状态（cursor-shape-v1 优先，主题回退，R10-3）。
    cursor: CursorManager,
    clipboard: Clipboard,
    connection: Connection,
    queue_handle: QueueHandle<Self>,
    /// `wl_compositor`：每个新窗口都从它 `create_surface`（多窗口的生命周期入口）。
    compositor: wl_compositor::WlCompositor,
    /// `xdg_wm_base`：每个新窗口都从它 `get_xdg_surface`；`ping` 必须回 pong。
    wm_base: xdg_wm_base::XdgWmBase,
    /// `wp_viewporter` 工厂：每个窗口要建自己的 `wp_viewport`（fractional 模式）。
    ///
    /// 工厂对象本身只在建子对象时使用，但仍要保留：多窗口下后续窗口的 viewport 都从它创建。
    viewporter: Option<WpViewporter>,
    /// `wp_fractional_scale_manager_v1` 工厂：每个窗口要建自己的 `wp_fractional_scale_v1`。
    ///
    /// 只有与 `viewporter` **同时**存在才启用 fractional 路径（V-110 / V-122 ④）。
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    /// `zxdg_decoration_manager_v1` 工厂：每个窗口要建自己的装饰对象（自绘标题栏）。
    decoration_manager: Option<ZxdgDecorationManagerV1>,
    seat: wl_seat::WlSeat,
    /// `wl_seat.capabilities` 最近一次声明的能力位：只有声明过的设备才能 `get_keyboard` / `get_pointer`
    /// （否则服务端报 `wl_seat error 0` 并断开连接，见 F-146）。
    seat_caps: wl_seat::Capability,
    keyboard: Option<wl_kb::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    /// 只为保活（`wl_data_device` 的 offer 事件在它上面）。
    _data_device: wl_data_device::WlDataDevice,
    /// `zwp_text_input_v3`（每 seat 一个对象；焦点路由见 `text_input_route`）。
    text_input: Option<ZwpTextInputV3>,
    /// text-input 的焦点路由 / 无 IM 降级状态机（R10-2 / R10-6 / R10-13）。
    text_input_route: TextInputRoute,
    /// 已绑定的 `wl_output`（容量 2：常见单 / 双屏；多屏时扩容，O(n) 扫描无所谓）。
    outputs: Vec<OutputEntry>,
    xkb_context: xkb::Context,
    xkb_state: Option<xkb::State>,
    /// 键盘布局的 keymap（`xkb_state` 的父对象）。
    ///
    /// 保留它是为了 `xkb_keymap_key_repeats`（R02-2 / R10-14）：Wayland 的按键重复由客户端
    /// 合成，"哪些键该重复"由布局决定（修饰键、锁定键不该重复）。`xkb::State` 不提供这个查询，
    /// 只有 `xkb::Keymap` 有。
    xkb_keymap: Option<xkb::Keymap>,
    /// 最近的输入 serial（IME / 剪贴板 / 拖拽都要）。
    serial: u32,
    /// 最近一次指针按键的 serial（`xdg_toplevel.move` / `resize` 的隐式抓取凭证）。
    pointer_serial: u32,
    /// 最近一次 `wl_pointer.enter` 的 serial（`set_cursor` 的**唯一**合法凭证，R10-3）。
    ///
    /// `wl_pointer.leave` 之后清空：协议规定光标变更只在「指针焦点是本客户端的 surface」时生效，
    /// 用离开前的旧 serial 下发既无效又可能被当作过期请求。
    pointer_enter_serial: Option<u32>,
    /// `wl_keyboard.modifiers` 的原始掩码：`(depressed, latched, locked, group)`。
    ///
    /// `group` 是**当前键盘布局组**。之前把它丢成恒 0，切换布局后字符仍按第一布局解析
    /// （P1-2.11）。xkbcommon 的 `update_mask` 把有效组放在 `locked_layout` 位置。
    mods: (u32, u32, u32, u32),
    modifiers: Modifiers,
    /// 键盘重复的参数（`wl_keyboard.repeat_info`；v4+）。Wayland 规定重复由**客户端**合成。
    key_repeat: KeyRepeat,
    /// 当前按下的键（用于合成重复）。顺序即按下顺序，释放时按 keycode 移除。
    held_keys: Vec<HeldKey>,
    /// 本帧尚未提交的滚轮累积（按 `wl_pointer.frame` 成批，P1-2.11）。
    scroll: ScrollAccum,
    /// 当前滚动是否来自**触控板 / 手指**（`wl_pointer.axis_source = finger`，R02-10）：
    /// 是 ⇒ 像素单位、连续阶段；否 ⇒ 行单位、离散滚轮。
    scroll_finger: bool,
    /// 连续滚动手势是否进行中（`axis_source` 之后、`axis_stop` 之前）。
    scroll_active: bool,
    pointer_pos: Point,
    /// 「需要下一次循环补一帧」的请求（可见性恢复 / 装饰模式变化 / 尺寸变化等）。
    ///
    /// 与 `App` 的唤醒位分开：这些变化发生在协议事件里，泵处理完后必须主动唤醒一次，
    /// 否则要等下一个无关事件才出帧。
    redraw: bool,
}

impl WaylandState {
    /// 连上 compositor、绑定必需 global、建首个 surface/toplevel、装 IME / 剪贴板（§8.1 的 1–4）。
    ///
    /// `size` / `title` 是应用建窗参数：compositor 的 configure 给出 `0×0` 时，客户端用请求的
    /// 尺寸作为初值（否则会退到固定的 800×600）。后续窗口（运行期新增 / 启动时就存在的）都走
    /// [`Self::add_window`]，本函数只在最后为**首个**窗口调用它一次。
    pub(crate) fn new(
        size: Size,
        title: &str,
        id: crate::window::WindowId,
    ) -> Result<(Self, wayland_client::EventQueue<Self>), EngineError> {
        // R10-16：断连 / 协议错误 / 缺 global 分成三种强类型失败（不再折叠成 AppInit 字符串）。
        let connection = Connection::connect_to_env().map_err(|error| EngineError::Wayland {
            kind: WaylandErrorKind::CompositorGone,
            detail: smol_str::format_smolstr!("no Wayland session: {error}"),
        })?;
        let (globals, queue) =
            registry_queue_init::<Self>(&connection).map_err(|error| EngineError::Wayland {
                kind: classify_global_error(&error),
                detail: smol_str::format_smolstr!("registry: {error}"),
            })?;
        let queue_handle = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            bind(&globals, &queue_handle, 4..=6, "wl_compositor")?;
        let wm_base: xdg_wm_base::XdgWmBase = bind(&globals, &queue_handle, 1..=6, "xdg_wm_base")?;
        let seat: wl_seat::WlSeat = bind(&globals, &queue_handle, 5..=9, "wl_seat")?;
        let data_device_manager: wl_data_device_manager::WlDataDeviceManager =
            bind(&globals, &queue_handle, 3..=3, "wl_data_device_manager")?;

        // 协议工厂对象**保留在 `WaylandState` 里**：多窗口下每个新窗口都要用它们建子对象
        // （`wp_viewport` / `wp_fractional_scale_v1` / `zxdg_toplevel_decoration_v1`）。
        // 绑定失败只是缺对应的可选扩展（`None` ⇒ 整数 scale 回退 / compositor 自选装饰）。
        let viewporter = globals
            .bind::<WpViewporter, _, _>(&queue_handle, 1..=1, ())
            .ok();
        let fractional_manager = globals
            .bind::<WpFractionalScaleManagerV1, _, _>(&queue_handle, 1..=1, ())
            .ok();
        let decoration_manager = globals
            .bind::<ZxdgDecorationManagerV1, _, _>(&queue_handle, 1..=2, ())
            .ok();
        // 光标：cursor-shape-v1（首选，免 buffer 管理）与 `wl_shm`（回退路径上传主题图像）。
        // 两者都是可选扩展：都没有时保持 compositor 的默认箭头（不报错）。
        let cursor_shape_manager = globals
            .bind::<WpCursorShapeManagerV1, _, _>(&queue_handle, 1..=2, ())
            .ok();
        let shm = globals
            .bind::<wl_shm::WlShm, _, _>(&queue_handle, 1..=1, ())
            .ok();

        let data_device = data_device_manager.get_data_device(&seat, &queue_handle, ());
        // **不在启动时创建 `wl_data_source`**（R10-1）：数据源是一次性的，每次 `set_selection`
        // 都要新建一个；启动时创建一个再复用正是「第二次复制后粘贴为空」的根因。

        let text_input = globals
            .bind::<ZwpTextInputManagerV3, _, _>(&queue_handle, 1..=1, ())
            .ok()
            .map(|manager| manager.get_text_input(&seat, &queue_handle, ()));

        let clipboard = Clipboard::new(
            data_device.clone(),
            data_device_manager,
            queue_handle.clone(),
        );
        let mut state = Self {
            windows: Vec::new(),
            // `windows` 里第一个窗口由下面的 `add_window` 补齐；此前没有事件派发。
            active: 0,
            pointer_focus: None,
            events: Vec::with_capacity(16),
            connection,
            queue_handle: queue_handle.clone(),
            compositor,
            wm_base,
            viewporter,
            fractional_manager,
            decoration_manager,
            seat,
            seat_caps: wl_seat::Capability::empty(),
            keyboard: None,
            pointer: None,
            _data_device: data_device,
            text_input,
            cursor: CursorManager::new(cursor_shape_manager, shm),
            text_input_route: TextInputRoute::default(),
            outputs: Vec::with_capacity(2),
            clipboard,
            xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
            xkb_keymap: None,
            serial: 0,
            pointer_serial: 0,
            pointer_enter_serial: None,
            mods: (0, 0, 0, 0),
            modifiers: Modifiers::empty(),
            key_repeat: KeyRepeat::default(),
            held_keys: Vec::new(),
            scroll: ScrollAccum::default(),
            scroll_finger: false,
            scroll_active: false,
            pointer_pos: Point::new(0.0, 0.0),
            redraw: false,
        };
        // 启动时绑定所有现存 `wl_output`：`initial_roundtrip` 的 2 次 roundtrip 会带回它们的
        // `wl_output.scale`，整数回退的首帧因此能按「全部已绑定 output 的最大 scale」兜底
        // （`wl_surface.enter` 要等 surface 映射之后才到，V-110）。
        state.bind_existing_outputs(&globals);
        state.add_window(id, size, title)?;
        Ok((state, queue))
    }

    /// 建一个原生窗口的协议对象，追加进 [`Self::windows`] 并**最后**触发首个 commit。
    ///
    /// 顺序是协议硬要求（R10-4，照 winit / Qt `createDecoration` 的做法）：
    ///
    /// ```text
    /// create_surface → get_xdg_surface → get_toplevel
    ///   → set_app_id / set_title / set_min_size
    ///   → get_toplevel_decoration + set_mode   ← xdg-decoration v1：必须早于首次 commit
    ///   → get_fractional_scale + get_viewport  ← 尽早创建，避免错过初始 preferred_scale
    ///   → surface.commit()                     ← 唯一触发初始 configure 的 commit
    /// ```
    ///
    /// 旧实现在 `set_min_size` 之后立刻 `commit()`，再创建 decoration —— xdg-decoration **v1**
    /// 明确把「toplevel 已 commit 之后创建 decoration」定义为 client error（`unconfigured_buffer`）。
    ///
    /// 首个 configure 由 compositor 异步发回，事件循环在收到 `configured` 之前不会为它建
    /// swapchain / 出帧。
    pub(crate) fn add_window(
        &mut self,
        id: crate::window::WindowId,
        size: Size,
        title: &str,
    ) -> Result<(), EngineError> {
        let surface = self.compositor.create_surface(&self.queue_handle, ());
        let xdg_surface = self
            .wm_base
            .get_xdg_surface(&surface, &self.queue_handle, ());
        let toplevel = xdg_surface.get_toplevel(&self.queue_handle, ());
        toplevel.set_app_id(APP_ID.to_owned());
        toplevel.set_title(title.to_owned());
        // Wayland 的 configure / window geometry 是整数，小数的逻辑尺寸在协议层没有表示：
        // 这里先四舍五入取整，整数回退下 buffer = `round(L × N)` 因而严格是 buffer_scale 的
        // 整数倍（否则 `wl_surface.attach` 报 `invalid_size`，V-110）。
        let size = Size::new(size.width.round().max(1.0), size.height.round().max(1.0));
        // 首帧尺寸：**xdg-shell 没有 `set_default_size` 请求**（协议原文的 xdg_toplevel 请求只有
        // set_parent/set_title/set_app_id/show_window_menu/move/resize/set_max_size/set_min_size/
        // set_maximized/unset_maximized/set_fullscreen/unset_fullscreen/set_minimized，
        // 见 `wayland-protocols-0.32.13/protocols/stable/xdg-shell/xdg-shell.xml:622-1247`）。
        // 初始尺寸因此只有两条合法途径：compositor 在首个 `configure` 里给尺寸，或它给 0×0 表示
        // 「客户端自选」—— 后者由下面的 `Configure` 分支保留本结构里的 `size`（我们不覆盖它）。
        // 最小尺寸是 window geometry 的逻辑像素（含 CSD 阴影边距），与 scale 无关：只需在首个
        // commit 前设置一次。
        window::apply_min_size(&toplevel);

        // 装饰：请求 CLIENT_SIDE（没有该扩展时 compositor 自行决定）。**必须在首次 commit
        // 之前**创建，`configure` 事件里的 `mode` 才是权威的最终模式（R10-4）。
        let decoration = self.decoration_manager.as_ref().map(|manager| {
            let decoration = manager.get_toplevel_decoration(&toplevel, &self.queue_handle, ());
            decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide);
            decoration
        });
        // 分数缩放只在 `wp_viewporter` 与 `wp_fractional_scale_manager_v1` **同时**存在时启用：
        // 没有 viewporter 就没有 destination，fractional 的 buffer 会被 compositor 按分数放大
        // surface 的逻辑尺寸（实测 1.25×），所以此时**不创建** fractional 对象，改走整数回退
        // （V-110 / V-122 ④）。fractional 对象与 viewport 对象因此成对存在。
        let fractional = if self.viewporter.is_some() {
            self.fractional_manager
                .as_ref()
                .map(|manager| manager.get_fractional_scale(&surface, &self.queue_handle, ()))
        } else {
            None
        };
        let viewport = match (&self.viewporter, &fractional) {
            (Some(viewporter), Some(_)) => {
                Some(viewporter.get_viewport(&surface, &self.queue_handle, ()))
            }
            _ => None,
        };
        let uses_viewport = viewport.is_some();
        self.windows.push(WindowSurface {
            id,
            surface,
            xdg_surface,
            toplevel,
            ime_cursor_area: None,
            ime_cursor_applied: None,
            ime_content_applied: None,
            ime_content: crate::window::ImeContentType::Text,
            ime: ImeState::new(),
            fractional,
            viewport,
            uses_viewport,
            decoration,
            decoration_mode: None,
            size,
            scale: Scale::ONE,
            surface_outputs: Vec::with_capacity(2),
            preferred_buffer_scale: None,
            fractional_scale_120: None,
            integer_scale: 1,
            entered_once: false,
            focused: false,
            configured: false,
            states: ToplevelStates::default(),
            wm_caps: WmCapabilities::ASSUMED,
            bounds: None,
            geometry: None,
            cursor: Cursor::Default,
            close_requested: false,
            frame_gate: FrameGate::new(),
            frame_callback: None,
        });
        // window geometry 与首个 commit 一起生效（`set_window_geometry` 是双缓冲状态）。
        self.apply_window_geometry(self.windows.len() - 1);
        // **唯一**触发初始 configure 的 commit：必须排在所有协议对象创建之后（R10-4）。
        self.windows[self.windows.len() - 1].surface.commit();
        Ok(())
    }

    /// 移除一个窗口（从 [`Self::windows`] 摘除；`WindowSurface` 的 `Drop` 按角色逆序销毁协议
    /// 对象，compositor 侧窗口随之消失）。
    ///
    /// 返回 `false` = 没有这个窗口。`active` / `pointer_focus` 都存的是**下标**，移除后
    /// 后面的窗口整体前移一位，必须按移除位置修正：被移除下标之前的焦点不变，之后的减一；
    /// 焦点正好是被移除窗口时夹紧到现存范围。指针进入的窗口被移除 ⇒ 指针已不可能在它上面，
    /// 置 `None`（它的 `wl_surface` 已销毁，后续 `leave` 也路由不到）。
    pub(crate) fn remove_window(&mut self, id: crate::window::WindowId) -> bool {
        let Some(index) = self.window_index(id) else {
            return false;
        };
        // 关窗时必须清空客户端合成的按键重复：之后不再有 `key` 事件（含 release），
        // `active` 又会重映射到别的窗口 ⇒ 按住键的重复会写进**另一个窗口**（复审 F5）。
        self.held_keys.clear();
        // R10-6：只有被移除的窗口**就是** IME 焦点时才停用 —— 旧实现在这里无条件
        // `set_ime_focus(false)`，于是关掉任意一个监控窗都会把焦点窗口的 IME 掐掉。
        if self.text_input_route.is_focus(index as u32) {
            self.apply_ime_target(None);
        }
        self.windows.remove(index);
        // text-input 路由存的是下标：移除之后整体前移，按位置修正（与 active 同一套规则）。
        self.text_input_route.reindex_removed(index as u32);
        if self.windows.is_empty() {
            self.active = 0;
            self.pointer_focus = None;
            return true;
        }
        if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            self.active = self.active.min(self.windows.len() - 1);
        }
        self.pointer_focus = match self.pointer_focus {
            Some(focus) if focus == index => None,
            Some(focus) if focus > index => Some(focus - 1),
            other => other,
        };
        true
    }

    /// 窗口个数（事件循环逐窗口遍历的边界）。
    pub(crate) fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// 下标 → 稳定 id（与 `App` 的 `WindowId` 同值）。
    pub(crate) fn window_id(&self, index: usize) -> crate::window::WindowId {
        self.windows[index].id
    }

    /// 稳定 id → 下标（`None` = 该窗口没有原生窗口）。
    pub(crate) fn window_index(&self, id: crate::window::WindowId) -> Option<usize> {
        self.windows.iter().position(|window| window.id == id)
    }

    /// 绑定所有现存 `wl_output`。
    ///
    /// `GlobalList::bind` 只绑第一个匹配的 global（wayland-client 文档明确 multi-instance
    /// global 应走 `WlRegistry::bind`），多显示器必须逐个实例绑定。
    fn bind_existing_outputs(&mut self, globals: &GlobalList) {
        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                self.bind_output(globals.registry(), global.name, global.version);
            }
        }
    }

    /// 动态绑定一个 `wl_output`（已绑定过的同名 global 幂等跳过）。
    ///
    /// 只绑到 v4：整数回退只需要 `scale`（v2）与 `release`（v3+），v4 带 `name` / `description`
    /// 便于诊断，再高的版本没有用处。
    fn bind_output(&mut self, registry: &wl_registry::WlRegistry, name: u32, version: u32) {
        if self.outputs.iter().any(|entry| entry.name == name) {
            return;
        }
        // `WlRegistry::bind` 返回 `WlOutput` 本身（`new_id` 由参数指定接口，无法失败到
        // `Result`；对象创建失败时是 inert 代理，永远收不到事件，等价于没有这个 output）。
        let output = registry.bind::<wl_output::WlOutput, _, _>(
            name,
            version.min(4),
            &self.queue_handle,
            name,
        );
        self.outputs.push(OutputEntry {
            output,
            name,
            scale: 1,
        });
    }

    /// `wl_registry.global_remove`：解绑并把它从 surface 的 enter 列表里移除。
    ///
    /// v1 / v2 的 `wl_output` 没有 `release` 请求：直接丢弃代理即可（服务端对象本身已随
    /// global 移除消失）。
    fn unbind_output(&mut self, name: u32) {
        let Some(index) = self.outputs.iter().position(|entry| entry.name == name) else {
            return;
        };
        let entry = self.outputs.remove(index);
        let id = entry.output.id();
        if entry.output.version() >= 3 {
            entry.output.release();
        }
        for index in 0..self.windows.len() {
            self.windows[index]
                .surface_outputs
                .retain(|entered| *entered != id);
        }
        // 一个 output 消失会影响**所有** enter 过它的窗口，逐个重算。
        self.recompute_all_scales();
    }

    /// 对所有窗口重算 scale（`wl_output` 层面的变化：output 移除 / output 的 scale 改变）。
    fn recompute_all_scales(&mut self) {
        for index in 0..self.windows.len() {
            self.recompute_scale(index);
        }
    }

    /// 整数回退当前应取的 scale（[`choose_integer_scale`] 的输入来自本状态）。
    ///
    /// `wl_surface.enter` 要等 surface 首次 attach/commit 之后才到，而首帧前已绑定的
    /// `wl_output.scale` 已经可用（sway 在 1.25 / 1.5 / 1.75 下都报 2），所以 `entered_once ==
    /// false` 时退到全部已绑定 output 的最大值；一旦收到过 enter，列表为空（最小化 / 切工作区）
    /// 就保留该窗口上一个 `integer_scale`，避免虚假的 1× ↔ 2× 抖动。
    fn integer_scale_value(&self, index: usize) -> i32 {
        let entered_max = self
            .outputs
            .iter()
            .filter(|entry| {
                self.windows[index]
                    .surface_outputs
                    .contains(&entry.output.id())
            })
            .map(|entry| entry.scale)
            .max();
        let bound_max = self.outputs.iter().map(|entry| entry.scale).max();
        choose_integer_scale(
            self.windows[index].preferred_buffer_scale,
            entered_max,
            bound_max,
            self.windows[index].integer_scale,
            self.windows[index].entered_once,
        )
    }

    /// 当前生效的 scale：fractional 模式下用 `preferred_scale`（未收到时回落到整数 scale），
    /// 否则就是整数回退的当前值。
    fn effective_scale(&self, index: usize) -> Scale {
        let fractional = if self.windows[index].uses_viewport {
            self.windows[index]
                .fractional_scale_120
                .and_then(|scale| dpi::scale_from_120(scale).ok())
        } else {
            None
        };
        fractional.unwrap_or_else(|| {
            Scale::new(self.windows[index].integer_scale as f32).expect("positive")
        })
    }

    /// 先推进整数回退的当前值，再按 effective scale 更新该窗口的 `scale` 并排队
    /// `Event::DpiChanged`（事件循环随后 `set_scale` / `set_surface` 并出帧）。
    fn recompute_scale(&mut self, index: usize) {
        self.windows[index].integer_scale = self.integer_scale_value(index);
        let scale = self.effective_scale(index);
        if scale == self.windows[index].scale {
            return;
        }
        self.windows[index].scale = scale;
        let window = self.windows[index].id;
        self.events.push((
            window,
            Event::DpiChanged {
                dpi: (scale.factor() * 96.0).round() as u32,
                scale,
            },
        ));
    }

    /// 渲染目标的 `wl_surface` buffer scale：fractional 路径恒 1（destination 负责非整数缩放），
    /// 整数回退是 [`Self::recompute_scale`] 当前选出的整数 scale（N=1 时也是 1）。
    ///
    /// GPU 线程在 present 前与 attach/commit 同一次生效（[`crate::render::frame::FrameRequest`]）。
    pub(crate) fn buffer_scale(&self, index: usize) -> i32 {
        if self.windows[index].uses_viewport {
            1
        } else {
            self.windows[index].integer_scale
        }
    }

    /// `wl_surface` 的代理副本：GPU 线程用它写 `set_buffer_scale`（`WlSurface` 是 `Clone`）。
    pub(crate) fn surface_clone(&self, index: usize) -> wl_surface::WlSurface {
        self.windows[index].surface.clone()
    }

    /// 指针当前进入的窗口 id（没有进入任何窗口时 None：Motion/Axis 仍然更新坐标，
    /// 但**不派发**事件 —— 指针在窗口外时没有目标）。
    fn pointer_window(&self) -> Option<crate::window::WindowId> {
        self.pointer_focus.map(|index| self.windows[index].id)
    }

    /// `wl_surface` → 窗口下标（按对象 id 比较；代理对象的 `id()` 是协议对象 id）。
    ///
    /// 事件归属**按对象解析**是 Wayland 客户端的正确做法：`Dispatch` 回调拿到的是发起
    /// 事件的那个代理对象，只有它能无歧义地指出「哪个窗口」。
    fn window_index_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        let id = surface.id();
        self.windows
            .iter()
            .position(|window| window.surface.id() == id)
    }

    /// `xdg_toplevel` → 窗口下标。
    fn window_index_for_toplevel(&self, toplevel: &xdg_toplevel::XdgToplevel) -> Option<usize> {
        let id = toplevel.id();
        self.windows
            .iter()
            .position(|window| window.toplevel.id() == id)
    }

    /// `wl_callback` → 窗口下标（按挂起的 `frame_callback` 对象 id 匹配）。
    fn window_index_for_callback(&self, callback: &wl_callback::WlCallback) -> Option<usize> {
        let id = callback.id();
        self.windows.iter().position(|window| {
            window
                .frame_callback
                .as_ref()
                .is_some_and(|(pending, _)| *pending == id)
        })
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    /// `wl_display*` / `wl_surface*`（Vulkan WSI 需要；`system` backend 的真实对象）。
    pub(crate) fn raw_pointers(
        &self,
        index: usize,
    ) -> (*mut std::ffi::c_void, *mut std::ffi::c_void) {
        (
            self.connection.display().id().as_ptr().cast(),
            self.windows[index].surface.id().as_ptr().cast(),
        )
    }

    pub(crate) fn size(&self, index: usize) -> Size {
        self.windows[index].size
    }

    pub(crate) fn scale(&self, index: usize) -> Scale {
        self.windows[index].scale
    }

    pub(crate) fn serial(&self) -> u32 {
        self.serial
    }

    pub(crate) fn is_configured(&self, index: usize) -> bool {
        self.windows[index].configured
    }

    pub(crate) fn is_focused(&self, index: usize) -> bool {
        self.windows[index].focused
    }

    /// 该窗口现在是否可以驱动帧（R10-7）。
    ///
    /// `wl_surface.frame` 的协议原文允许服务端在 surface 不可见时**不**回 callback，因此
    /// 客户端必须自己判定「不可见就既不请求帧、也不启动 200 ms 活性兜底」—— 否则隐藏/遮挡的
    /// 窗口会退化成永久 5 Hz 的全量渲染 + buffer commit。
    pub(crate) fn is_visible(&self, index: usize) -> bool {
        let window = &self.windows[index];
        toplevel::is_visible(Visibility {
            configured: window.configured,
            suspended: window.states.suspended,
            entered_once: window.entered_once,
            outputs: window.surface_outputs.len(),
        })
    }

    /// 取走「需要补一帧」的标记（可见性恢复 / 装饰模式变化 / 尺寸变化后由泵唤醒一次）。
    pub(crate) fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// 该窗口是否需要引擎自绘标题栏（R10-4）。
    ///
    /// compositor 说 `server_side` 时引擎必须停止自绘，否则出现双标题栏。
    pub(crate) fn draws_client_decoration(&self, index: usize) -> bool {
        self.windows[index]
            .decoration_mode
            .is_none_or(DecorationMode::draws_client_decoration)
    }

    /// 把 window geometry 下发给 `xdg_surface`（同值不重复发，R10-10）。
    ///
    /// 参数是 surface-local 逻辑像素，扣除 CSD 阴影边距（当前无阴影 ⇒ 整个 surface）。
    /// window geometry 是**双缓冲状态**：本次调用在 GPU 线程的下一次 commit 里生效，
    /// 因此它必须发生在 `request_frame`/`app.frame` 之前（`add_window` / configure 里都是）。
    fn apply_window_geometry(&mut self, index: usize) {
        let geometry = toplevel::window_geometry(self.windows[index].size, window::SHADOW_MARGIN);
        if self.windows[index].geometry == Some(geometry) {
            return;
        }
        self.windows[index].geometry = Some(geometry);
        self.windows[index].xdg_surface.set_window_geometry(
            geometry.x,
            geometry.y,
            geometry.width,
            geometry.height,
        );
    }

    /// 把 `wp_viewport` 的所有权交给窗口渲染器（`Some` = fractional 模式）。
    ///
    /// 只有 `wp_viewporter` 与 `wp_fractional_scale_manager_v1` **同时**存在时才创建 viewport
    /// （[`Self::buffer_scale`] 因此恒为 1）；`None` ⇒ 整数回退：buffer 按整数 scale 放大、
    /// `wl_surface.set_buffer_scale` 由 GPU 线程在同一次 present 前写入，不需要 destination
    /// （V-110）。take 之后 [`Self::uses_viewport`] 仍为 `true`，buffer scale 不会退回整数路径。
    ///
    /// destination 只能与 buffer 同一次 present 生效：旧实现由 UI 线程单独 `set_destination` +
    /// `wl_surface.commit()`（未 attach 新 buffer）会让 compositor 立刻把旧 buffer 按新 destination
    /// 缩放，而且这次 commit 会提前触发 frame callback、破坏 FrameGate。现在 UI 线程不再碰
    /// viewport（也不再 commit `wl_surface`）。
    pub(crate) fn take_viewport(&mut self, index: usize) -> Option<WpViewport> {
        self.windows[index].viewport.take()
    }

    pub(crate) fn take_events(&mut self) -> Vec<(crate::window::WindowId, Event)> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn take_close_request(&mut self, index: usize) -> bool {
        std::mem::take(&mut self.windows[index].close_requested)
    }

    /// 最小化（自绘标题栏的“缩小”按钮）。
    ///
    /// `wm_capabilities` 未声明 `minimize` 时不下发（R10-15：compositor 会忽略，但我们不该
    /// 假装按钮有效；应用可通过 `Window::state()` 的 `can_minimize` 隐藏按钮）。
    pub(crate) fn minimize(&self, index: usize) {
        if !self.windows[index].wm_caps.minimize {
            return;
        }
        self.windows[index].toplevel.set_minimized();
    }

    /// 最大化 / 还原切换；最终状态以 compositor 的 configure 为准。
    pub(crate) fn toggle_maximize(&mut self, index: usize) {
        if !self.windows[index].wm_caps.maximize {
            return;
        }
        if self.windows[index].states.maximized {
            self.windows[index].toplevel.unset_maximized();
        } else {
            self.windows[index].toplevel.set_maximized();
        }
    }

    /// 进入 / 退出全屏（R10-15）。
    ///
    /// 与最大化不同，全屏在 tiling compositor 上是独立语义；`set_fullscreen(None)` 表示「由
    /// compositor 选输出」（多数 compositor 用指针/焦点所在的输出）。最终状态同样以
    /// `configure` 的 `Fullscreen` 状态位为准（`states.fullscreen`）。
    pub(crate) fn toggle_fullscreen(&mut self, index: usize) {
        if !self.windows[index].wm_caps.fullscreen {
            return;
        }
        if self.windows[index].states.fullscreen {
            self.windows[index].toplevel.unset_fullscreen();
        } else {
            self.windows[index].toplevel.set_fullscreen(None);
        }
    }

    /// compositor 侧的窗口状态镜像（应用自绘标题栏据此决定画什么，R10-4 / R10-15）。
    pub(crate) fn window_state(&self, index: usize) -> crate::window::WindowState {
        let window = &self.windows[index];
        crate::window::WindowState {
            client_decoration: self.draws_client_decoration(index),
            maximized: window.states.maximized,
            fullscreen: window.states.fullscreen,
            can_maximize: window.wm_caps.maximize,
            can_minimize: window.wm_caps.minimize,
            can_fullscreen: window.wm_caps.fullscreen,
        }
    }

    /// 开始交互式移动；使用触发拖动的指针按键 serial。
    pub(crate) fn begin_move(&self, index: usize) {
        self.windows[index]
            .toplevel
            ._move(&self.seat, self.pointer_serial);
    }

    /// 开始交互式缩放；使用触发拖动的指针按键 serial。
    pub(crate) fn begin_resize(&self, index: usize, edge: crate::window::WindowResizeEdge) {
        use wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;

        use crate::window::WindowResizeEdge;

        let edge = match edge {
            WindowResizeEdge::Top => ResizeEdge::Top,
            WindowResizeEdge::Bottom => ResizeEdge::Bottom,
            WindowResizeEdge::Left => ResizeEdge::Left,
            WindowResizeEdge::Right => ResizeEdge::Right,
            WindowResizeEdge::TopLeft => ResizeEdge::TopLeft,
            WindowResizeEdge::TopRight => ResizeEdge::TopRight,
            WindowResizeEdge::BottomLeft => ResizeEdge::BottomLeft,
            WindowResizeEdge::BottomRight => ResizeEdge::BottomRight,
        };
        self.windows[index]
            .toplevel
            .resize(&self.seat, self.pointer_serial, edge);
    }

    pub(crate) fn clipboard_mut(&mut self) -> &mut Clipboard {
        &mut self.clipboard
    }

    /// 请求一帧回调（publish 之前调用；已有挂起 callback 时只关好门禁，不重复请求）。
    pub(crate) fn request_frame(&mut self, index: usize) {
        let Some(token) = self.windows[index].frame_gate.request(Instant::now()) else {
            return;
        };
        let callback = self.windows[index].surface.frame(&self.queue_handle, ());
        self.windows[index].frame_callback = Some((callback.id(), token));
    }

    pub(crate) fn frame_ready(&self, index: usize) -> bool {
        self.windows[index].frame_gate.is_ready()
    }

    /// 本帧没有 publish（Idle / 中止）⇒ 不会 commit：把门禁重新打开（F-183 的 Idle 路径）。
    pub(crate) fn note_frame_not_committed(&mut self, index: usize) {
        self.windows[index].frame_gate.note_not_committed();
    }

    /// 活性兜底：可见窗口里门禁关闭超过 `GATE_TIMEOUT` 时强制打开。
    ///
    /// 只覆盖「publish 了但没 commit」（present `OUT_OF_DATE` 等），正常路径不会触发；
    /// **不是**呈现节奏（F-183）。**不可见的窗口不参与**（R10-7）：协议允许服务端在 surface
    /// 不可见时不回 callback，若继续计时，隐藏窗口会永远以 5 Hz 全量重绘（并持续 commit）。
    /// 不可见窗口的门禁被直接置为 Idle（不留下「关闭中」的状态与计时）。
    pub(crate) fn expire_frame_gates(&mut self, now: Instant) -> bool {
        let mut expired = false;
        for index in 0..self.windows.len() {
            if !self.is_visible(index) {
                self.windows[index].frame_gate.force_idle();
                continue;
            }
            expired |= self.windows[index].frame_gate.expire(now);
        }
        expired
    }

    /// 可见窗口里最早的活性兜底期限（门禁全开时 `None`）；事件循环用它压缩阻塞等待。
    pub(crate) fn frame_gate_remaining(&self, now: Instant) -> Option<Duration> {
        (0..self.windows.len())
            .filter(|index| self.is_visible(*index))
            .filter_map(|index| self.windows[index].frame_gate.time_until_expiry(now))
            .min()
    }

    /// 把 text-input 焦点切到 `target`（`None` = 停用），按**差分**下发 `enable`/`disable`。
    ///
    /// 协议要求（R10-6 / R10-13）：
    /// - 换焦点前必须先 `disable` 旧的（XML `enable`：*must disable the current text input before
    ///   enabling the new one*）；
    /// - `done` 之前的 `set_cursor_rectangle` / `set_content_type` 都是双缓冲状态，必须
    ///   `commit()` 才生效；
    /// - `leave` 之后 compositor 会忽略本对象的所有请求，直到下一次 `enter` —— 所以焦点不匹配
    ///   时**不**下发状态（由 [`TextInputRoute::focus`] 判定）。
    fn apply_ime_target(&mut self, target: Option<usize>) {
        let Some(text_input) = self.text_input.clone() else {
            return;
        };
        let change = match target {
            Some(index) => self.text_input_route.enter(index as u32, Instant::now()),
            None => self.text_input_route.disable(),
        };
        if !change.disable && !change.enable {
            return;
        }
        if change.disable {
            text_input.disable();
        }
        if change.enable
            && let Some(index) = target
        {
            text_input.enable();
            // XML `set_content_type` / `set_cursor_rectangle`：状态「保持到下一次 commit 的
            // enable/disable 为止」⇒ 新 enable 之后必须重发（缓存清空）。
            self.windows[index].ime_cursor_applied = None;
            self.windows[index].ime_content_applied = None;
            self.push_ime_state(index, &text_input);
        }
        text_input.commit();
        self.text_input_route.note_commit();
    }

    /// 下发该窗口的 IME 状态（caret 矩形 + `content_type`），变化才发（R10-12）。
    ///
    /// 调用方保证当前 text-input 焦点就是这个窗口（`leave` 之后的请求会被 compositor 忽略）。
    fn push_ime_state(&mut self, index: usize, text_input: &ZwpTextInputV3) {
        let area = self.windows[index]
            .ime_cursor_area
            .unwrap_or_else(|| Rect::new(0.0, 0.0, 1.0, 1.0));
        if self.windows[index].ime_cursor_applied != Some(area) {
            text_input.set_cursor_rectangle(
                area.origin.x.round() as i32,
                area.origin.y.round() as i32,
                area.size.width.max(1.0).round() as i32,
                area.size.height.max(1.0).round() as i32,
            );
            self.windows[index].ime_cursor_applied = Some(area);
        }
        // 密码 / API key 输入框：`hidden_text | sensitive_data`（输入法不得存词库）。
        let content = self.windows[index].ime_content;
        let bits = text_input::content_type_bits(content);
        if self.windows[index].ime_content_applied != Some(bits) {
            text_input.set_content_type(
                zwp_text_input_v3::ContentHint::from_bits_truncate(bits.hint),
                zwp_text_input_v3::ContentPurpose::try_from(bits.purpose)
                    .unwrap_or(zwp_text_input_v3::ContentPurpose::Normal),
            );
            self.windows[index].ime_content_applied = Some(bits);
        }
    }

    /// 保存 IME caret 区域；焦点正是这个窗口时立即下发（`commit` 是 v3 的生效点）。
    fn set_ime_cursor_area(
        &mut self,
        index: usize,
        area: Option<Rect>,
        content: crate::window::ImeContentType,
    ) {
        let content_changed = self.windows[index].ime_content != content;
        self.windows[index].ime_content = content;
        let area_changed = self.windows[index].ime_cursor_area != area;
        self.windows[index].ime_cursor_area = area;
        if !area_changed && !content_changed {
            return;
        }
        // 只有当前 text-input 焦点才发请求：`leave` 之后 compositor 会忽略一切请求。
        if self.text_input_route.focus() != Some(index as u32) {
            return;
        }
        let Some(text_input) = self.text_input.clone() else {
            return;
        };
        self.push_ime_state(index, &text_input);
        text_input.commit();
        // `commit()` 次数是 `done.serial` 的期望值；不计数就无法在 `done` 里判断「compositor
        // 是否已经应用了我们上一批状态」（R10-13 问题 2）。
        self.text_input_route.note_commit();
    }

    /// 从 `App` 同步指定窗口的 IME caret 区域与 `content_type`（每轮泵逐窗口调用）。
    ///
    /// 独立于帧门禁：候选窗定位与密码框语义不能等下一帧。
    pub(crate) fn sync_ime_cursor_area(&mut self, app: &App, window: crate::window::WindowId) {
        let Some(index) = self.window_index(window) else {
            return;
        };
        let content = app
            .window(window)
            .map_or(crate::window::ImeContentType::Text, |window| {
                window.ime_content_type()
            });
        let area = match app.ime_cursor_area(window) {
            Ok(area) => area,
            // 文本尚未 shape / 文本刚变化：保留上一次有效区域（但 `content_type` 仍要更新）。
            Err(_) => self.windows[index].ime_cursor_area,
        };
        self.set_ime_cursor_area(index, area, content);
    }

    /// 无输入法守护进程时的降级判定（R10-2）；返回 `true` ⇒ 文本通路已切回键盘。
    ///
    /// 判据在 [`TextInputRoute`]：`enable` 之后 1.5 s 内没有任何 IM 事件 ⇒ 本 seat 没有引擎。
    pub(crate) fn degrade_text_input_if_silent(&mut self, now: Instant) -> bool {
        if !self.text_input_route.should_degrade(now) {
            return false;
        }
        // 组合态必须一起丢掉：回到键盘通路后 `composing` 残留会永久抑制文本。
        if let Some(index) = self.text_input_route.focus()
            && let Some(window) = self.windows.get_mut(index as usize)
        {
            window.ime.reset();
        }
        self.redraw = true;
        true
    }

    /// 距离无 IM 降级判定还有多久（事件循环据此压缩等待；`None` = 不需要等）。
    pub(crate) fn next_ime_degrade_in(&self, now: Instant) -> Option<Duration> {
        self.text_input_route.next_degrade_in(now)
    }

    /// 应用请求切换光标形状（`App::take_cursor_change` 的消费点，R10-3）。
    ///
    /// 形状按**窗口**记账（同一窗口里的控件状态可能不同），指针进入时立即重放窗口的形状；
    /// 只有指针正停在这个窗口上时才会真正下发 `set_shape` / `set_cursor`（其余情况等 enter）。
    pub(crate) fn set_cursor_shape(&mut self, window: crate::window::WindowId, cursor: Cursor) {
        let Some(index) = self.window_index(window) else {
            return;
        };
        if self.windows[index].cursor == cursor {
            return;
        }
        self.windows[index].cursor = cursor;
        if self.pointer_focus == Some(index) {
            self.apply_cursor(index);
        }
    }

    /// 把该窗口的当前光标形状真正下发（enter 时与形状变化时各一次）。
    fn apply_cursor(&mut self, index: usize) {
        let Some(serial) = self.pointer_enter_serial else {
            return;
        };
        let cursor = self.windows[index].cursor;
        let scale = if self.windows[index].uses_viewport {
            // 分数缩放：用向上取整的整数倍选主题图（回退路径没有 compositor 代劳）。
            self.windows[index].scale.factor().ceil().max(1.0) as i32
        } else {
            self.windows[index].integer_scale.max(1)
        };
        let pointer = self.pointer.clone();
        let compositor = self.compositor.clone();
        let queue = self.queue_handle.clone();
        self.cursor
            .apply(pointer.as_ref(), serial, cursor, scale, &compositor, &queue);
    }

    /// 指针离开所有 surface：隐藏光标（R10-3；否则光标会残留在别的窗口上）。
    fn hide_cursor(&mut self) {
        let serial = self.pointer_enter_serial.take();
        let pointer = self.pointer.clone();
        if let Some(serial) = serial {
            self.cursor.hide(pointer.as_ref(), serial);
        }
    }

    /// 键盘：`wl_keyboard::Event::Key` → `Event`（字符输入 + 非字符键）。
    ///
    /// `timestamp` 是事件时刻：`wl_keyboard.key` 带 `time`（毫秒），合成重复用引擎时钟。
    /// `repeat` 由调用方明确给出（R02-2）：真实按下为 `false`，客户端合成的重复为 `true`。
    fn translate_key(&mut self, key: u32, state: KeyState, repeat: bool, timestamp: Instant) {
        let Some(xkb_state) = self.xkb_state.as_mut() else {
            return;
        };
        let (depressed, latched, locked, group) = self.mods;
        // 有效布局组放在 `locked_layout` 位置（Wayland 只给有效组，smithay/winit 同此约定）。
        xkb_state.update_mask(depressed, latched, locked, 0, 0, group);
        let keycode = xkb::Keycode::new(key);
        // seat 事件没有 surface 归属：派发给**焦点窗口**（active）。
        let window = self.windows[self.active].id;
        self.events.push((
            window,
            Event::Input(InputEvent::Key {
                // R02-1：物理键身份取自 **evdev 码**（布局无关），不是 keysym（布局相关）。
                code: key_code(key.saturating_sub(8)),
                state,
                repeat,
                modifiers: self.modifiers,
                timestamp,
            }),
        ));
        // R10-2：文本是否**只**走 IME 由 `TextInputRoute` 决定 —— `enable` 之后 1.5 s 内没有
        // 任何 IM 事件就降级回键盘通路（否则没有输入法守护进程的 compositor 上字符输入全灭）。
        // 组合态（preedit 非空）里物理按键不产生正文：组合串由 `ImeEvent::Preedit` 表达。
        //
        // R02-2（P0 回归）：**`repeat` 不得参与文本判定**。Wayland 的按键重复由客户端合成
        // （服务端从不重发 `key` 事件），字符文本也只能由客户端按每次重复生成；旧实现写成
        // `!repeat` ⇒ 在默认的 XKB 回退通路里按住可打印键只输入一个字符。判据统一走
        // `should_emit_text`（含单测），`repeat` 只影响绑定/动作派发。
        let composing = self.windows[self.active].ime.composing();
        let ime_takes_text = self.text_input_route.takes_text();
        // R10-5：Ctrl / Alt / Super 组合不产生字符（`key_get_utf8` 不看这些修饰键，
        // `Ctrl+C` 会拿到 "c"）。Shift 与 AltGr（Mod5，`Modifiers` 无对应位）不受影响。
        if should_emit_text(state, repeat, ime_takes_text, composing)
            && !text_input::text_blocked_by_modifiers(self.modifiers)
        {
            let text = xkb_state.key_get_utf8(keycode);
            if !text.is_empty() && text.chars().all(|c| !c.is_control()) {
                self.events.push((
                    window,
                    Event::Input(InputEvent::Text {
                        text: SmolStr::new(text),
                        timestamp,
                    }),
                ));
            }
        }
    }

    /// 记录一次按键的按下/释放状态（合成重复用，P1-2.11）。
    fn note_key_held(&mut self, xkb_key: u32, key_state: KeyState) {
        match key_state {
            KeyState::Down => {
                if !self.held_keys.iter().any(|held| held.key == xkb_key) {
                    self.held_keys.push(HeldKey {
                        key: xkb_key,
                        next: std::time::Instant::now()
                            + std::time::Duration::from_millis(u64::from(self.key_repeat.delay)),
                    });
                }
            }
            KeyState::Up => self.held_keys.retain(|held| held.key != xkb_key),
        }
    }

    /// 该 xkb keycode 是否支持重复（`xkb_keymap_key_repeats`，R02-2 / R10-14）。
    ///
    /// Wayland 的重复由**客户端**合成，但"哪些键该重复"由键盘布局决定：布告栏里的
    /// `repeat` 属性（`xkb_keymap_key_repeats`）把修饰键、锁定键排除在外。不查它就会让
    /// 按住 Ctrl 不断产生 Ctrl 的重复事件（winit / GTK 都查这个函数）。
    fn key_repeats(&self, xkb_key: u32) -> bool {
        self.xkb_keymap
            .as_ref()
            .is_none_or(|keymap| keymap.key_repeats(xkb::Keycode::new(xkb_key)))
    }

    /// `repeat_info` 变化后重排全部按住键的下次触发时刻。
    fn reschedule_held_keys(&mut self) {
        let now = std::time::Instant::now();
        let delay = std::time::Duration::from_millis(u64::from(self.key_repeat.delay));
        for held in &mut self.held_keys {
            held.next = now + delay;
        }
    }

    /// 合成到期的重复按键（把 Key Down + Text 推进 `events`）。
    ///
    /// 由事件循环每轮调用；`rate == 0` 时不做任何事（服务端要求关闭重复）。
    /// 产物带 `repeat = true`（R02-2）：引擎的绑定派发会忽略它，只有默认的导航/滚动行为允许重复。
    pub(crate) fn emit_due_key_repeats(&mut self, now: std::time::Instant) {
        if self.held_keys.is_empty() {
            return;
        }
        let Some(interval) = self.key_repeat.interval() else {
            return;
        };
        // 收集到期的 keycode，避免在迭代 `held_keys` 时可变借用 `self`。
        let mut due: Vec<u32> = Vec::new();
        for held in &mut self.held_keys {
            if held.next <= now {
                due.push(held.key);
                // 按固定间隔推进（不是 `now + interval`）：系统卡顿后不补偿爆发式重复，
                // 多拍只合成一次 —— 这与 GTK/Qt 的客户端重复实现一致。
                held.next = now + interval;
            }
        }
        for key in due {
            // R02-2：不支持重复的键（修饰键 / 锁定键）不合成 —— 判据是 xkb 的 keymap。
            if self.key_repeats(key) {
                self.translate_key(key, KeyState::Down, true, now);
            }
        }
    }

    /// 距离下一次需要合成重复还有多久（事件循环据此决定 `dispatch` 的 timeout）。
    ///
    /// `None` = 没有按住键或重复已关闭 ⇒ 不缩短等待。
    pub(crate) fn next_key_repeat_in(
        &self,
        now: std::time::Instant,
    ) -> Option<std::time::Duration> {
        // 重复关闭 ⇒ `None`（不缩短等待）。
        self.key_repeat.interval()?;
        self.held_keys
            .iter()
            .map(|held| held.next.saturating_duration_since(now))
            .min()
    }

    fn update_modifiers(&mut self) {
        self.modifiers = Modifiers::empty();
        let (depressed, latched, locked, group) = self.mods;
        let Some(state) = self.xkb_state.as_mut() else {
            return;
        };
        state.update_mask(depressed, latched, locked, 0, 0, group);
        let effective = xkb::STATE_MODS_EFFECTIVE;
        if state.mod_name_is_active(xkb::MOD_NAME_SHIFT, effective) {
            self.modifiers |= Modifiers::SHIFT;
        }
        if state.mod_name_is_active(xkb::MOD_NAME_CTRL, effective) {
            self.modifiers |= Modifiers::CONTROL;
        }
        if state.mod_name_is_active(xkb::MOD_NAME_ALT, effective) {
            self.modifiers |= Modifiers::ALT;
        }
        if state.mod_name_is_active(xkb::MOD_NAME_LOGO, effective) {
            self.modifiers |= Modifiers::META;
        }
    }

    fn pointer_button(button: u32) -> Option<PointerButton> {
        match button {
            0x110 => Some(PointerButton::Primary),
            0x111 => Some(PointerButton::Middle),
            0x112 => Some(PointerButton::Secondary),
            0x113 => Some(PointerButton::X1),
            0x114 => Some(PointerButton::X2),
            _ => None,
        }
    }

    /// 把已排队的协议事件分发进 handler（`initial_roundtrip` 与首帧之前调用）。
    fn dispatch_pending(
        &mut self,
        queue: &mut wayland_client::EventQueue<Self>,
    ) -> Result<(), EngineError> {
        queue
            .dispatch_pending(self)
            .map(|_| ())
            .map_err(|error| EngineError::Wayland {
                kind: classify_dispatch_error(&error),
                detail: smol_str::format_smolstr!("dispatch_pending: {error}"),
            })
    }
}

/// wayland-client 的后端错误 → 强类型分类（R10-16）。
///
/// 后端已经把两类失败分开：`Io` 是连接断了（`EPIPE` / socket 关闭），`Protocol` 是协议错误。
/// 运行期（`calloop` 的 `dispatch`）由 `calloop-wayland-source` 把 `Io` 原样转成 `io::Error`、
/// 把协议错误转成 `Errno::PROTO`，分类同样落到 [`WaylandErrorKind::from_io_error`]。
fn classify_backend_error(error: &wayland_client::backend::WaylandError) -> WaylandErrorKind {
    match error {
        wayland_client::backend::WaylandError::Io(io) => WaylandErrorKind::from_io_error(io),
        wayland_client::backend::WaylandError::Protocol(_) => WaylandErrorKind::Protocol,
    }
}

/// `GlobalList` 初始化失败 → 强类型分类。
fn classify_global_error(error: &wayland_client::globals::GlobalError) -> WaylandErrorKind {
    match error {
        wayland_client::globals::GlobalError::Backend(backend) => classify_backend_error(backend),
        wayland_client::globals::GlobalError::InvalidId(_) => WaylandErrorKind::Protocol,
    }
}

/// wayland-client 的调度失败 → 强类型分类。
fn classify_dispatch_error(error: &wayland_client::DispatchError) -> WaylandErrorKind {
    match error {
        // 收到的消息不符合接口签名（服务端/客户端协议版本不匹配）。
        wayland_client::DispatchError::BadMessage { .. } => WaylandErrorKind::Protocol,
        wayland_client::DispatchError::Backend(backend) => classify_backend_error(backend),
    }
}

/// 绑定一个必需 global（缺失 / 版本不足 ⇒ [`WaylandErrorKind::MissingGlobal`]）。
fn bind<P>(
    globals: &GlobalList,
    queue: &QueueHandle<WaylandState>,
    range: std::ops::RangeInclusive<u32>,
    name: &'static str,
) -> Result<P, EngineError>
where
    P: Proxy + 'static,
    WaylandState: Dispatch<P, ()>,
{
    globals
        .bind::<P, _, _>(queue, range, ())
        .map_err(|error| EngineError::Wayland {
            kind: WaylandErrorKind::MissingGlobal,
            detail: smol_str::format_smolstr!("{name}: {error:?}"),
        })
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // `wl_output` 是热插拔的多实例 global：新出现的实例动态绑定（热插拔显示器）。
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_output" {
                    state.bind_output(registry, name, version);
                }
            }
            wl_registry::Event::GlobalRemove { name } => state.unbind_output(name),
            _ => {}
        }
    }
}

macro_rules! dispatch_unit {
    ($($proxy:ty),* $(,)?) => {
        $(
            impl Dispatch<$proxy, ()> for WaylandState {
                fn event(
                    _: &mut Self,
                    _: &$proxy,
                    _: <$proxy as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

dispatch_unit!(
    wl_compositor::WlCompositor,
    wl_data_device_manager::WlDataDeviceManager,
    wl_shm::WlShm,
    // 光标回退路径会 `wl_shm.create_pool` + `wl_shm_pool.create_buffer`：两个子对象都要求
    // `WaylandState: Dispatch<_, ()>`（`create_*` 的 `qh` 参数），否则编译不过。
    // `wl_shm_pool` 没有事件；`wl_buffer` 只有 `release` —— 本仓自己缓存并销毁 buffer，
    // 因此忽略 `release`（`dispatch_unit!` 的空 handler 正是这个语义）。
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    WpFractionalScaleManagerV1,
    WpViewporter,
    WpViewport,
    ZwpTextInputManagerV3,
    ZxdgDecorationManagerV1,
    WpCursorShapeManagerV1,
    WpCursorShapeDeviceV1,
);

impl Dispatch<wl_output::WlOutput, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 只关心整数 scale（`name` 是 bind 时写入的 registry global name，与 entry 一一对应）；
        // geometry / mode / name / description 都不影响渲染。
        if let wl_output::Event::Scale { factor } = event {
            if let Some(entry) = state.outputs.iter_mut().find(|entry| entry.name == *name) {
                entry.scale = factor.max(1);
            }
            state.recompute_all_scales();
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            // `WEnum::Unknown` 是未来版本新增的位：按「本版本不认识」处理，保留已知能力。
            state.seat_caps = match capabilities {
                wayland_client::WEnum::Value(caps) => caps,
                wayland_client::WEnum::Unknown(_) => state.seat_caps,
            };
            // 能力可能在建好设备之后才到齐（部分 compositor 首帧之后才 announce），
            // 也可能被撤销（热插拔）。`create_devices` 是幂等的：补建缺失的、释放多余的。
            let _ = create_devices(state);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for WaylandState {
    fn event(
        state: &mut Self,
        surface: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 窗口归属按对象解析：enter / leave / preferred_buffer_scale 都是**那个 surface**
        // 的属性（scale 协商状态在 Stage C-1 已下移到 `WindowSurface`）。
        let Some(index) = state.window_index_for_surface(surface) else {
            return;
        };
        // 整数回退的 scale 来源：surface v6 的 `preferred_buffer_scale` 优先，否则取已 enter 的
        // output 最大 scale（多屏窗口跨两个不同 scale 的显示器时取较大者，避免糊）。
        match event {
            wl_surface::Event::Enter { output } => {
                let id = output.id();
                if !state.windows[index].surface_outputs.contains(&id) {
                    state.windows[index].surface_outputs.push(id);
                }
                // 从这一刻起整数 scale 只信已 enter 的 output（为空时保留上一个值）。
                state.windows[index].entered_once = true;
                state.recompute_scale(index);
                // R10-7：重新可见 ⇒ 门禁与帧驱动恢复，主动补一帧（不可见期间我们不请求帧）。
                let visible = state.is_visible(index);
                if visible {
                    state.redraw = true;
                }
            }
            wl_surface::Event::Leave { output } => {
                let id = output.id();
                state.windows[index]
                    .surface_outputs
                    .retain(|entered| *entered != id);
                state.recompute_scale(index);
                // 离开全部 output ⇒ 不可见：门禁回到 Idle（不保留兜底计时，R10-7）。
                if !state.is_visible(index) {
                    state.windows[index].frame_gate.force_idle();
                }
            }
            wl_surface::Event::PreferredBufferScale { factor } => {
                state.windows[index].preferred_buffer_scale = Some(factor.max(1));
                state.recompute_scale(index);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for WaylandState {
    fn event(
        state: &mut Self,
        callback: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            // 窗口归属**按对象解析**，不靠 `active`：`wl_callback` 可能属于任一窗口，
            // 而 `active` 是键盘焦点窗口（两者在多窗口下并不相同）。
            let Some(index) = state.window_index_for_callback(callback) else {
                return;
            };
            let Some((pending, token)) = state.windows[index].frame_callback.as_ref() else {
                return;
            };
            // 被活性兜底放弃的旧 callback 迟到：对象 id 不匹配，不能动当前请求的挂起状态
            // （`expire` 之后重新挂上的 callback 才是门禁要等的那一个）。
            if pending != &callback.id() {
                return;
            }
            let token = *token;
            state.windows[index].frame_callback = None;
            state.windows[index].frame_gate.on_callback(token);
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            // 必须回 pong，否则 compositor 判定客户端无响应。
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for WaylandState {
    fn event(
        _: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for WaylandState {
    fn event(
        state: &mut Self,
        toplevel: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 窗口归属按对象解析（`Configure` / `Close` 只描述发起它的那个 toplevel）。
        let Some(index) = state.window_index_for_toplevel(toplevel) else {
            return;
        };
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let previous = state.windows[index].size;
                // 0 = 「客户端自己选」；否则用 compositor 给的建议尺寸。
                if width > 0 && height > 0 {
                    state.windows[index].size = Size::new(width as f32, height as f32);
                }
                // `states` 是协议数组的原始字节（u32 native-endian 的 state 枚举序列）。
                state.windows[index].states = ToplevelStates::parse(&states);
                let was_configured = state.windows[index].configured;
                state.windows[index].configured = true;
                let size = state.windows[index].size;
                let window = state.windows[index].id;
                // R10-10：window geometry 随尺寸变化更新（与下一次 commit 一同生效）。
                state.apply_window_geometry(index);
                // R10-18：`xdg_toplevel.configure` 在任何状态变化时都会来（焦点、平铺、resize），
                // 尺寸没变就不要推 `Resized` —— 否则消费者会看到大量重复 resize。
                if size != previous || !was_configured {
                    state.events.push((window, Event::Resized { size }));
                }
                // 可见性可能刚变化（`Suspended` 出现/消失）：恢复可见时要主动补一帧。
                let visible = state.is_visible(index);
                state.redraw |= visible;
            }
            xdg_toplevel::Event::WmCapabilities { capabilities } => {
                // R10-15：compositor 声明的能力（枚举值列表，不是位掩码）。
                let next = WmCapabilities::parse(&capabilities);
                if state.windows[index].wm_caps != next {
                    state.windows[index].wm_caps = next;
                    state.redraw = true;
                }
            }
            xdg_toplevel::Event::ConfigureBounds { width, height } => {
                // R10-15：建议的尺寸上限（扣掉面板等区域）。落成 `set_max_size`，
                // 避免最大化时向 compositor 请求超出可用工作区的尺寸。
                if width > 0 && height > 0 {
                    let bounds = Size::new(width as f32, height as f32);
                    if state.windows[index].bounds != Some(bounds) {
                        state.windows[index].bounds = Some(bounds);
                        state.windows[index].toplevel.set_max_size(
                            bounds.width.round() as i32,
                            bounds.height.round() as i32,
                        );
                    }
                } else {
                    state.windows[index].bounds = None;
                }
            }
            xdg_toplevel::Event::Close => {
                state.windows[index].close_requested = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        fractional: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && scale != 0
        {
            // 归属按对象解析（每个窗口有自己的 `wp_fractional_scale_v1`）。
            let id = fractional.id();
            let Some(index) = state
                .windows
                .iter()
                .position(|window| window.fractional.as_ref().is_some_and(|f| f.id() == id))
            else {
                return;
            };
            state.windows[index].fractional_scale_120 = Some(scale);
            state.recompute_scale(index);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if matches!(
                    format,
                    wayland_client::WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
                ) {
                    // SAFETY: 协议保证 `fd` 是可读的 keymap；`new_from_fd` 接管其所有权。
                    let keymap = unsafe {
                        xkb::Keymap::new_from_fd(
                            &state.xkb_context,
                            fd,
                            size as usize,
                            xkb::KEYMAP_FORMAT_TEXT_V1,
                            xkb::COMPILE_NO_FLAGS,
                        )
                    };
                    if let Ok(Some(keymap)) = keymap {
                        // R02-2：keymap 要留着做 `xkb_keymap_key_repeats`（见 `key_repeats`）。
                        state.xkb_state = Some(xkb::State::new(&keymap));
                        state.xkb_keymap = Some(keymap);
                    }
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                state.mods = (mods_depressed, mods_latched, mods_locked, group);
                state.update_modifiers();
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: key_state,
                ..
            } => {
                state.serial = serial;
                let key_state = match key_state {
                    wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed) => {
                        Some(KeyState::Down)
                    }
                    wayland_client::WEnum::Value(wl_keyboard::KeyState::Released) => {
                        Some(KeyState::Up)
                    }
                    _ => None,
                };
                if let Some(key_state) = key_state {
                    // evdev 码 + 8 = xkb keycode。
                    let xkb_key = key + 8;
                    state.note_key_held(xkb_key, key_state);
                    // R02-14：`wl_keyboard.key` 的 `time` 是 compositor 的毫秒时钟，原点未定义
                    // （协议只保证单调），无法与 `Instant` 对齐 ⇒ 用分发时刻并**在此标注**：
                    // 键盘事件的时刻精度因此是本轮泵的取出时刻（Wayland 的已知限制）。
                    state.translate_key(xkb_key, key_state, false, Instant::now());
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                // Wayland 规定重复由客户端合成（P1-2.11）。`rate == 0` 表示服务端要求关闭重复。
                state.key_repeat = KeyRepeat {
                    rate: u32::try_from(rate).unwrap_or(0),
                    delay: u32::try_from(delay).unwrap_or(0),
                };
                // 参数变化后按新参数重排所有按住键的下次触发时刻。
                state.reschedule_held_keys();
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.serial = serial;
                // 焦点窗口由 `enter` 的 surface 决定（Stage C-2）：seat 事件此后都归它。
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.active = index;
                state.windows[index].focused = true;
                let window = state.windows[index].id;
                state
                    .events
                    .push((window, Event::FocusChanged { focused: true }));
                // R10-6：IME 焦点跟随键盘焦点（zwp_text_input_v3.enter 到达后会被去重成 no-op）。
                state.apply_ime_target(Some(index));
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                // 失焦后不再有 `wl_keyboard.key`（含 release）到达：必须**主动清空**按住键，
                // 否则客户端合成的重复会永久继续（winit/GTK 同此约定）。后果是静默的：
                // 按住 Backspace 切走 → 后台继续删字；关窗后 `active` 重映射 → 重复键写进
                // **另一个窗口**。
                //
                // 先清再解析窗口：surface 可能已经被移除（关窗流程里 `leave` 晚到），
                // 旧实现直接 `return` 把按键状态永久留在表里（复审 F5）。
                state.held_keys.clear();
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.windows[index].focused = false;
                let window = state.windows[index].id;
                state
                    .events
                    .push((window, Event::FocusChanged { focused: false }));
                // R10-6：只有失焦的正是当前 IME 焦点时才停用（差分下发在 apply_ime_target）。
                if state.text_input_route.is_focus(index as u32) {
                    state.apply_ime_target(None);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                state.serial = serial;
                // 指针进入的窗口由 surface 决定；之后的指针事件都派发给它（与键盘焦点无关）。
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.pointer_focus = Some(index);
                // R10-3：`set_cursor` 只用 enter 的 serial（协议：光标变更只对已获得焦点的
                // surface 生效，enter 的 serial 是新焦点下第一个合法凭证）。
                state.pointer_enter_serial = Some(serial);
                state.pointer_pos = Point::new(surface_x as f32, surface_y as f32);
                let window = state.windows[index].id;
                // 进入时立即重放该窗口的光标形状（否则 compositor 会一直显示它自己的默认箭头）。
                state.apply_cursor(index);
                // R02-4：`wl_pointer.enter` 是进窗事件（与 Win32 的 TrackMouseEvent 首次移动、
                // AppKit 的 `mouseEntered:` 对齐）；之后的 `motion` 才是纯移动。
                state.events.push((
                    window,
                    Event::Input(InputEvent::PointerEnter {
                        position: state.pointer_pos,
                        modifiers: state.modifiers,
                        timestamp: Instant::now(),
                    }),
                ));
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_pos = Point::new(surface_x as f32, surface_y as f32);
                if let Some(window) = state.pointer_window() {
                    state.events.push((
                        window,
                        Event::Input(InputEvent::PointerMove {
                            position: state.pointer_pos,
                            modifiers: state.modifiers,
                            timestamp: Instant::now(),
                        }),
                    ));
                }
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: button_state,
                ..
            } => {
                if let Some(button) = Self::pointer_button(button) {
                    let timestamp = Instant::now();
                    let event = match button_state {
                        wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed) => {
                            state.pointer_serial = serial;
                            InputEvent::PointerDown {
                                position: state.pointer_pos,
                                button,
                                modifiers: state.modifiers,
                                timestamp,
                            }
                        }
                        _ => InputEvent::PointerUp {
                            position: state.pointer_pos,
                            button,
                            modifiers: state.modifiers,
                            timestamp,
                        },
                    };
                    if let Some(window) = state.pointer_window() {
                        state.events.push((window, Event::Input(event)));
                    }
                }
            }
            wl_pointer::Event::AxisSource { axis_source } => {
                // R02-10：`axis_source` 区分「滚轮 / 手指 / 连续设备」——只有手指（触控板）才是
                // 像素级 1:1 跟随 + 连续阶段；滚轮是离散行。
                state.scroll_finger = matches!(
                    axis_source,
                    wayland_client::WEnum::Value(wl_pointer::AxisSource::Finger)
                );
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                // 连续值（触控板即像素；v5 起滚轮一格 = 10 个单位）。**不能**除 120 ——
                // 那是 `axis_value120` 的量纲（P1-2.11）。
                match axis {
                    wayland_client::WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                        state.scroll.continuous_x += value as f32;
                    }
                    _ => state.scroll.continuous_y += value as f32,
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                // v8+ 的高精度离散步长：1/120 行，与 Win32 的 `WHEEL_DELTA = 120` 同量纲。
                match axis {
                    wayland_client::WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                        state.scroll.discrete_x = state.scroll.discrete_x.saturating_add(value120);
                    }
                    _ => state.scroll.discrete_y = state.scroll.discrete_y.saturating_add(value120),
                }
            }
            wl_pointer::Event::AxisStop { .. } => {
                // 一次连续手势结束：补一条 `phase = End` 的空增量事件，引擎据此结束惯性/动画
                // （Chrome 的 `WebMouseWheelEvent.phase = kScrollPhaseEnded` 同义）。
                state.scroll_active = false;
                if let Some(window) = state.pointer_window() {
                    state.events.push((
                        window,
                        Event::Input(scroll_event(
                            state.pointer_pos,
                            (0.0, 0.0),
                            state.scroll_finger,
                            ScrollPhase::End,
                            state.modifiers,
                        )),
                    ));
                }
            }
            wl_pointer::Event::Frame => {
                // 一次 frame = 一批同属一个逻辑输入的指针事件 ⇒ 只产出一个 Scroll。
                // 未实现 `axis_value120` 的服务端（v8 以下）只会来连续 `axis`，两条通路在此合并。
                if !state.scroll.is_empty() {
                    // 手指（触控板）：连续值就是像素增量，1:1 跟随；滚轮：换算成行。
                    let (delta_x, delta_y) = if state.scroll_finger {
                        let (x, y) = state.scroll.continuous_pixels();
                        (-x, -y)
                    } else {
                        let (x, y) = state.scroll.lines();
                        (-x, -y)
                    };
                    state.scroll.clear();
                    let phase = if state.scroll_finger {
                        let phase = if state.scroll_active {
                            ScrollPhase::Update
                        } else {
                            ScrollPhase::Begin
                        };
                        state.scroll_active = true;
                        phase
                    } else {
                        ScrollPhase::Discrete
                    };
                    if let Some(window) = state.pointer_window() {
                        state.events.push((
                            window,
                            Event::Input(scroll_event(
                                state.pointer_pos,
                                (delta_x, delta_y),
                                state.scroll_finger,
                                phase,
                                state.modifiers,
                            )),
                        ));
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                // R10-3：离开时隐藏光标 —— 否则指针移到别的 surface 上时，compositor 会继续
                // 用我们最后一次设置的图像（协议允许，但观感上就是「光标卡住了」）。
                state.hide_cursor();
                let window = state.pointer_window();
                state.pointer_focus = None;
                if let Some(window) = window {
                    state.events.push((
                        window,
                        Event::Input(InputEvent::PointerLeave {
                            position: state.pointer_pos,
                            modifiers: state.modifiers,
                            timestamp: Instant::now(),
                        }),
                    ));
                    // wayland 的隐式 grab 正常期间不会发 `leave`，所以多数拖拽仍会收到 release；
                    // 但 grab 被合成器撤销（屏幕锁定、Super 概览、别的客户端抢 grab、Alt+Tab 的
                    // 某些实现）时会**先收到 leave 而没有 release**。此时 app 侧
                    // `on_pointer_leave` 在捕获态下保留会话 ⇒ 拖拽永久停在「按住」，
                    // 之后所有按下都被忽略（复审 P2-7）。R02-8 要求把这条语义明确成
                    // `PointerCancel`：它在无捕获时是幂等 no-op（只清 hover），有捕获时结束会话
                    // 并派发 `DragPhase::Cancel`，与 Windows 的 `WM_CAPTURECHANGED` 路径对称。
                    state.events.push((
                        window,
                        Event::Input(InputEvent::PointerCancel {
                            timestamp: Instant::now(),
                        }),
                    ));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `accept` 的凭证（R10-25）：用最近的输入 serial，而不是恒 0。
        state.clipboard.handle_device_event(event, state.serial);
    }

    // `wl_data_device.data_offer` 会创建 `wl_data_offer` 子对象：wayland-client 要求在这里
    // 给出它的 user data，缺了会在**收到第一个 offer 时 panic**（mutter 启动即发，F-147）。
    event_created_child!(WaylandState, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for WaylandState {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.clipboard.handle_offer_event(offer, event);
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for WaylandState {
    fn event(
        state: &mut Self,
        data_source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                state.clipboard.handle_send(data_source, &mime_type, fd);
            }
            // 对端换了选区属主：这个 source 立即作废，必须 destroy（R10-1）。
            wl_data_source::Event::Cancelled => state.clipboard.handle_cancelled(data_source),
            _ => {}
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // R10-13：`enter` / `leave` 是 text-input 焦点的**权威**信号（不是 `wl_keyboard`）。
        // compositor 在 `leave` 之后会忽略本对象的一切请求，直到下一次 `enter`，所以路由必须
        // 落在事件携带的 surface 上，而不是「当前活动窗口」。
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.apply_ime_target(Some(index));
            }
            zwp_text_input_v3::Event::Leave { surface } => {
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                // XML：`The client should reset any preedit string previously set`。
                state.windows[index].ime.reset();
                if state.text_input_route.is_focus(index as u32) {
                    state.apply_ime_target(None);
                }
            }
            zwp_text_input_v3::Event::PreeditString { text, .. } => {
                state.text_input_route.note_im_event();
                if let Some(index) = state.text_input_route.focus() {
                    state.windows[index as usize].ime.note_preedit(text);
                }
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                state.text_input_route.note_im_event();
                if let Some(index) = state.text_input_route.focus() {
                    state.windows[index as usize].ime.note_commit(text);
                }
            }
            zwp_text_input_v3::Event::DeleteSurroundingText { .. } => {
                // 有 IM 引擎的证据（R10-2 的降级判据）——即使我们暂时无法应用删除。
                state.text_input_route.note_im_event();
                // `ImeEvent` 目前没有「删除周围文本」的变体（`crate::event::ImeEvent` 由输入
                // 波次维护）：本平台**没有** `set_surrounding_text` 通路，因此输入法不会合法地
                // 发出这条事件；真收到时只能忽略（见 R10-12 的收尾说明）。
            }
            zwp_text_input_v3::Event::Done { serial } => {
                // 文本变化恒应用（XML）；serial 只决定「是否可以下发新状态」。
                let matches = state.text_input_route.settle(serial);
                let Some(index) = state.text_input_route.focus() else {
                    return;
                };
                let index = index as usize;
                let window = state.windows[index].id;
                for event in state.windows[index].ime.take_events() {
                    state.events.push((window, Event::Ime(event as ImeEvent)));
                }
                if matches {
                    // serial 对上：把攒下的状态（caret / content_type）发出去（R10-13 问题 2）。
                    if let Some(text_input) = state.text_input.clone() {
                        state.push_ime_state(index, &text_input);
                        text_input.commit();
                        state.text_input_route.note_commit();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        decoration: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // R10-4：`configure` 的 `mode` **必须被遵从**（XML：*The specified mode must be obeyed
        // by the client.*）。compositor 选 server_side 时引擎必须停止自绘标题栏，否则双标题栏。
        let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event else {
            return;
        };
        let id = decoration.id();
        let Some(index) = state
            .windows
            .iter()
            .position(|window| window.decoration.as_ref().is_some_and(|d| d.id() == id))
        else {
            return;
        };
        let mode = match mode {
            wayland_client::WEnum::Value(mode) => {
                toplevel::DecorationMode::from_protocol(mode as u32)
                    .unwrap_or(DecorationMode::ClientSide)
            }
            // 未来版本的枚举值：保留当前模式（不猜测 compositor 的意图）。
            wayland_client::WEnum::Unknown(value) => {
                DecorationMode::from_protocol(value).unwrap_or(DecorationMode::ClientSide)
            }
        };
        if state.windows[index].decoration_mode != Some(mode) {
            state.windows[index].decoration_mode = Some(mode);
            // 装饰模式变化会改变客户区高度与标题栏绘制：必须重排一帧。
            state.redraw = true;
        }
    }
}

/// 初始化后一次 roundtrip（拿首个 configure + 分数缩放 / 整数 scale）。
///
/// 2 次 roundtrip 会带回已绑定 `wl_output` 的 `scale`；`wl_surface.enter` 与
/// `preferred_buffer_scale` 可能晚于首帧到达，整数回退此时用「全部已绑定 output 的最大 scale」
/// 兜底（收到过 enter 之后改用已 enter 的最大值 / 上一个值），到达后 `DpiChanged` 触发一帧
/// （V-110）。
pub(crate) fn initial_roundtrip(
    queue: &mut wayland_client::EventQueue<WaylandState>,
    state: &mut WaylandState,
) -> Result<(), EngineError> {
    for _ in 0..2 {
        queue
            .roundtrip(state)
            .map_err(|error| EngineError::Wayland {
                kind: classify_dispatch_error(&error),
                detail: smol_str::format_smolstr!("roundtrip: {error}"),
            })?;
    }
    state.dispatch_pending(queue)?;
    // 启动路径上 `new` 只建了首个窗口（下标 0）；首帧之前它必须拿到 configure（V-110）。
    if !state.is_configured(0) {
        return Err(EngineError::Wayland {
            kind: WaylandErrorKind::SurfaceLost,
            detail: smol_str::SmolStr::new_static("no xdg_toplevel.configure before first frame"),
        });
    }
    Ok(())
}

/// 把 `wl_keyboard` / `wl_pointer` 建出来（或按最新的 capabilities 释放）。
///
/// **只有 seat 声明过的能力才能 bind**：`wl_seat.get_keyboard` 在没有 KEYBOARD 能力时是协议错误，
/// 服务端会回 `wl_seat error 0` 并直接断开连接（F-146）。幂等：每轮 capabilities 变化都调用。
///
/// 能力撤销时**必须 `release()`**（R10-8）：直接丢客户端代理只会让 wayland-client 把对象从本地
/// registry 摘掉，服务端对象仍然存在并继续发事件 —— 对未知对象 id 的事件是协议错误
/// （`BadMessage`），最终表现为整个进程以「calloop dispatch: …」退出。`wl_keyboard` / `wl_pointer`
/// 从 v3 起才有 `release`，v1/v2 只能丢代理（老 compositor 上能力撤销极罕见）。
pub(crate) fn create_devices(state: &mut WaylandState) -> Result<(), EngineError> {
    let queue = state.queue_handle.clone();
    let caps = state.seat_caps;
    if caps.contains(wl_seat::Capability::Keyboard) {
        if state.keyboard.is_none() {
            state.keyboard = Some(state.seat.get_keyboard(&queue, ()));
        }
    } else {
        // 键盘能力被撤销：之后再也不会有 key release，必须清空按住键，
        // 否则客户端合成的重复会永远跑下去（复审 F5）。
        state.held_keys.clear();
        if let Some(keyboard) = state.keyboard.take() {
            // IME 焦点也随键盘一起失效（没有键盘就没有文本焦点）。
            state.apply_ime_target(None);
            if keyboard.version() >= 3 {
                keyboard.release();
            }
        }
    }
    if caps.contains(wl_seat::Capability::Pointer) {
        if state.pointer.is_none() {
            let pointer = state.seat.get_pointer(&queue, ());
            // 光标通道绑定在 pointer 对象上：新建 pointer 时必须重建 shape device（R10-3）。
            state.cursor.pointer_created(&pointer, &queue);
            state.pointer = Some(pointer);
        }
    } else if let Some(pointer) = state.pointer.take() {
        state.cursor.pointer_gone();
        state.pointer_enter_serial = None;
        state.pointer_focus = None;
        if pointer.version() >= 3 {
            pointer.release();
        }
    }
    Ok(())
}

/// 空闲等待的兜底超时（`ping` / 协议事件会提前返回；3600 s 只是防御）。
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);

/// 一条滚动事件的构造（`unit` / `phase` 两处共用，避免手抄漏字段）。
///
/// `finger` = 触控板 / 连续设备（`axis_source = finger`）：像素单位、连续阶段；否则是离散滚轮，
/// 行单位、`ScrollPhase::Discrete`。
fn scroll_event(
    position: Point,
    delta: (f32, f32),
    finger: bool,
    phase: ScrollPhase,
    modifiers: Modifiers,
) -> InputEvent {
    InputEvent::Scroll {
        position,
        delta: Point::new(delta.0, delta.1),
        unit: if finger {
            ScrollUnit::Pixel
        } else {
            ScrollUnit::Line
        },
        phase,
        modifiers,
        timestamp: Instant::now(),
    }
}

/// `evdev` 键码 → 引擎 [`KeyCode`]（W3C `KeyboardEvent.code` 语义，R02-1）。
///
/// **必须用 evdev 码而不是 keysym**：`xkb_state.key_get_one_sym()` 返回的是**布局相关**的
/// keysym（AZERTY 上物理 `Q` 位置的 keysym 是 `q`），按它做快捷键就跨不了布局；evdev 码是硬件
/// 位置，与键盘布局无关。这样同一个物理键在三平台都是同一个 `KeyCode`（Linux `KEY_S` = 31 ⇒
/// `KeyS`、Windows 扫描码 0x1F ⇒ `KeyS`、macOS `kVK_ANSI_S` ⇒ `KeyS`）。
///
/// 未覆盖的码返回 [`KeyCode::Unidentified`]（载荷是 evdev 码，文档明确**不可跨平台**）。
fn key_code(evdev: u32) -> KeyCode {
    match evdev {
        1 => KeyCode::Escape,
        2 => KeyCode::Digit1,
        3 => KeyCode::Digit2,
        4 => KeyCode::Digit3,
        5 => KeyCode::Digit4,
        6 => KeyCode::Digit5,
        7 => KeyCode::Digit6,
        8 => KeyCode::Digit7,
        9 => KeyCode::Digit8,
        10 => KeyCode::Digit9,
        11 => KeyCode::Digit0,
        12 => KeyCode::Minus,
        13 => KeyCode::Equal,
        14 => KeyCode::Backspace,
        15 => KeyCode::Tab,
        16 => KeyCode::KeyQ,
        17 => KeyCode::KeyW,
        18 => KeyCode::KeyE,
        19 => KeyCode::KeyR,
        20 => KeyCode::KeyT,
        21 => KeyCode::KeyY,
        22 => KeyCode::KeyU,
        23 => KeyCode::KeyI,
        24 => KeyCode::KeyO,
        25 => KeyCode::KeyP,
        26 => KeyCode::BracketLeft,
        27 => KeyCode::BracketRight,
        28 => KeyCode::Enter,
        29 => KeyCode::ControlLeft,
        30 => KeyCode::KeyA,
        31 => KeyCode::KeyS,
        32 => KeyCode::KeyD,
        33 => KeyCode::KeyF,
        34 => KeyCode::KeyG,
        35 => KeyCode::KeyH,
        36 => KeyCode::KeyJ,
        37 => KeyCode::KeyK,
        38 => KeyCode::KeyL,
        39 => KeyCode::Semicolon,
        40 => KeyCode::Quote,
        41 => KeyCode::Backquote,
        42 => KeyCode::ShiftLeft,
        43 => KeyCode::Backslash,
        44 => KeyCode::KeyZ,
        45 => KeyCode::KeyX,
        46 => KeyCode::KeyC,
        47 => KeyCode::KeyV,
        48 => KeyCode::KeyB,
        49 => KeyCode::KeyN,
        50 => KeyCode::KeyM,
        51 => KeyCode::Comma,
        52 => KeyCode::Period,
        53 => KeyCode::Slash,
        54 => KeyCode::ShiftRight,
        55 => KeyCode::NumpadMultiply,
        56 => KeyCode::AltLeft,
        57 => KeyCode::Space,
        58 => KeyCode::CapsLock,
        59 => KeyCode::F1,
        60 => KeyCode::F2,
        61 => KeyCode::F3,
        62 => KeyCode::F4,
        63 => KeyCode::F5,
        64 => KeyCode::F6,
        65 => KeyCode::F7,
        66 => KeyCode::F8,
        67 => KeyCode::F9,
        68 => KeyCode::F10,
        69 => KeyCode::NumLock,
        70 => KeyCode::ScrollLock,
        71 => KeyCode::Numpad7,
        72 => KeyCode::Numpad8,
        73 => KeyCode::Numpad9,
        74 => KeyCode::NumpadSubtract,
        75 => KeyCode::Numpad4,
        76 => KeyCode::Numpad5,
        77 => KeyCode::Numpad6,
        78 => KeyCode::NumpadAdd,
        79 => KeyCode::Numpad1,
        80 => KeyCode::Numpad2,
        81 => KeyCode::Numpad3,
        82 => KeyCode::Numpad0,
        83 => KeyCode::NumpadDecimal,
        86 => KeyCode::IntlBackslash,
        87 => KeyCode::F11,
        88 => KeyCode::F12,
        89 => KeyCode::IntlRo,
        96 => KeyCode::NumpadEnter,
        97 => KeyCode::ControlRight,
        98 => KeyCode::NumpadDivide,
        99 => KeyCode::PrintScreen,
        100 => KeyCode::AltRight,
        102 => KeyCode::Home,
        103 => KeyCode::ArrowUp,
        104 => KeyCode::PageUp,
        105 => KeyCode::ArrowLeft,
        106 => KeyCode::ArrowRight,
        107 => KeyCode::End,
        108 => KeyCode::ArrowDown,
        109 => KeyCode::PageDown,
        110 => KeyCode::Insert,
        111 => KeyCode::Delete,
        117 => KeyCode::NumpadEqual,
        119 => KeyCode::Pause,
        121 => KeyCode::NumpadComma,
        124 => KeyCode::IntlYen,
        138 => KeyCode::Help,
        139 => KeyCode::ContextMenu,
        125 => KeyCode::MetaLeft,
        126 => KeyCode::MetaRight,
        127 => KeyCode::ContextMenu,
        // 媒体 / 浏览器 / 电源键（与 Windows 的 `E0` 扩展行同一组身份）。
        113 => KeyCode::AudioVolumeMute,
        114 => KeyCode::AudioVolumeDown,
        115 => KeyCode::AudioVolumeUp,
        163 => KeyCode::MediaTrackNext,
        164 => KeyCode::MediaPlayPause,
        165 => KeyCode::MediaTrackPrevious,
        166 => KeyCode::MediaStop,
        362 => KeyCode::MediaSelect,
        155 => KeyCode::LaunchMail,
        148 => KeyCode::LaunchApp1,
        149 => KeyCode::LaunchApp2,
        158 => KeyCode::BrowserBack,
        159 => KeyCode::BrowserForward,
        172 => KeyCode::BrowserHome,
        173 => KeyCode::BrowserRefresh,
        128 => KeyCode::BrowserStop,
        217 => KeyCode::BrowserSearch,
        156 => KeyCode::BrowserFavorites,
        116 => KeyCode::Power,
        142 => KeyCode::Sleep,
        143 => KeyCode::WakeUp,
        183 => KeyCode::F13,
        184 => KeyCode::F14,
        185 => KeyCode::F15,
        186 => KeyCode::F16,
        187 => KeyCode::F17,
        188 => KeyCode::F18,
        189 => KeyCode::F19,
        190 => KeyCode::F20,
        191 => KeyCode::F21,
        192 => KeyCode::F22,
        193 => KeyCode::F23,
        194 => KeyCode::F24,
        other => KeyCode::Unidentified(other as u16),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONTINUOUS_AXIS_UNITS_PER_LINE, KeyRepeat, ScrollAccum, choose_integer_scale, key_code,
        scroll_event,
    };
    use crate::{
        event::{KeyCode, Modifiers, ScrollPhase, ScrollUnit},
        math::Point,
    };

    /// P1-2.11：`axis_value120` 是 1/120 行，一格（120）= 整整一行。
    ///
    /// 旧实现把连续的 `axis`（一格 10–15 个单位）除以 120，于是滚轮慢约一个数量级。
    #[test]
    fn discrete_scroll_is_one_line_per_120_units() {
        let one_notch = ScrollAccum {
            discrete_y: 120,
            ..ScrollAccum::default()
        };
        assert_eq!(one_notch.lines().1, 1.0, "一格 = 120/120 = 1 行");

        let two_notches = ScrollAccum {
            discrete_y: 240,
            ..ScrollAccum::default()
        };
        assert_eq!(two_notches.lines().1, 2.0, "两格 = 2 行");
    }

    /// 连续 `axis`（滚轮一格 = 10 个单位）也必须得到 1 行，而不是 10/120 ≈ 0.083 行。
    #[test]
    fn continuous_scroll_uses_the_documented_unit() {
        let one_notch = ScrollAccum {
            continuous_y: CONTINUOUS_AXIS_UNITS_PER_LINE,
            ..ScrollAccum::default()
        };
        assert_eq!(
            one_notch.lines().1,
            1.0,
            "一格连续值 = 1 行（旧实现是 0.083）"
        );
    }

    /// 两条通路在同一 frame 内叠加（服务端可能同时发 `axis` 与 `axis_value120`）。
    #[test]
    fn both_axis_paths_accumulate() {
        let mixed = ScrollAccum {
            discrete_x: 60,
            continuous_x: CONTINUOUS_AXIS_UNITS_PER_LINE,
            ..ScrollAccum::default()
        };
        assert_eq!(mixed.lines().0, 1.5, "0.5 行离散 + 1 行连续");
    }

    #[test]
    fn an_empty_accumulator_reports_empty() {
        let mut scroll = ScrollAccum::default();
        assert!(scroll.is_empty());
        scroll.continuous_y = -3.0;
        assert!(!scroll.is_empty());
        scroll.clear();
        assert!(scroll.is_empty(), "clear 之后必须回到空态");
    }

    /// `repeat_info` 的 `rate == 0` = 服务端要求关闭重复。
    #[test]
    fn zero_repeat_rate_disables_repeats() {
        assert!(
            KeyRepeat {
                rate: 0,
                delay: 500
            }
            .interval()
            .is_none()
        );
        assert_eq!(
            KeyRepeat {
                rate: 25,
                delay: 500
            }
            .interval(),
            Some(std::time::Duration::from_millis(40)),
            "25 Hz ⇒ 40 ms 间隔"
        );
    }

    /// 默认参数必须在服务端未发 `repeat_info` 时也产生合理重复。
    #[test]
    fn default_repeat_is_enabled_and_reasonable() {
        let interval = KeyRepeat::default().interval().expect("默认必须启用重复");
        assert!(
            interval >= std::time::Duration::from_millis(20)
                && interval <= std::time::Duration::from_millis(100),
            "默认重复间隔应在 20–100 ms（实际 {interval:?}）"
        );
    }

    #[test]
    fn preferred_buffer_scale_wins_over_outputs() {
        assert_eq!(
            choose_integer_scale(Some(3), Some(2), Some(2), 1, true),
            3,
            "preferred_buffer_scale 优先于已 enter / 已绑定 / 上一个值"
        );
        assert_eq!(
            choose_integer_scale(Some(1), Some(2), None, 2, true),
            1,
            "preferred 为 1 时也优先（不取更大的 output scale）"
        );
    }

    #[test]
    fn entered_outputs_win_over_bound_outputs() {
        assert_eq!(
            choose_integer_scale(None, Some(2), Some(1), 1, true),
            2,
            "已 enter 的最大 scale 优先于全部已绑定 output 的最大值"
        );
    }

    #[test]
    fn before_the_first_enter_the_bound_max_is_a_conservative_estimate() {
        assert_eq!(
            choose_integer_scale(None, None, Some(2), 1, false),
            2,
            "enter 尚未到达时用全部已绑定 output 的最大 scale（单屏 HiDPI 首帧即正确）"
        );
    }

    #[test]
    fn after_the_first_enter_an_empty_list_keeps_the_previous_scale() {
        assert_eq!(
            choose_integer_scale(None, None, Some(2), 1, true),
            1,
            "leave 清空后保留上一个值，避免 1× ↔ 2× 的虚假 DPI 变化"
        );
    }

    #[test]
    fn no_scale_information_at_all_falls_back_to_one() {
        assert_eq!(choose_integer_scale(None, None, None, 1, false), 1);
        assert_eq!(choose_integer_scale(None, None, None, 1, true), 1);
    }

    #[test]
    fn a_non_positive_scale_is_clamped_to_one() {
        assert_eq!(choose_integer_scale(Some(0), None, None, 2, false), 1);
        assert_eq!(choose_integer_scale(None, Some(-2), Some(2), 1, true), 1);
        assert_eq!(choose_integer_scale(None, None, Some(0), 1, false), 1);
        assert_eq!(choose_integer_scale(None, None, None, 0, true), 1);
        assert_eq!(choose_integer_scale(None, None, None, -3, true), 1);
    }

    /// R02-1：物理键身份取自 **evdev 码**（布局无关），不是 keysym（布局相关）。
    ///
    /// AZERTY 上物理 `Q` 位置的 keysym 是 `q`、键位码仍是 `KEY_Q`(16) ⇒ 三平台都得到
    /// `KeyCode::KeyQ`，同一张 `KeyBinding` 表跨布局成立。
    #[test]
    fn physical_key_identity_is_layout_independent() {
        assert_eq!(key_code(31), KeyCode::KeyS);
        assert_eq!(key_code(30), KeyCode::KeyA);
        assert_eq!(key_code(16), KeyCode::KeyQ);
        assert_eq!(key_code(96), KeyCode::NumpadEnter);
        assert_eq!(key_code(28), KeyCode::Enter);
        assert_eq!(key_code(105), KeyCode::ArrowLeft);
        assert_eq!(key_code(58), KeyCode::CapsLock);
        assert_eq!(key_code(42), KeyCode::ShiftLeft);
        assert_eq!(key_code(54), KeyCode::ShiftRight);
        // 未覆盖的码：载荷是 evdev 码，文档明确**不可跨平台**。
        assert_eq!(key_code(4242), KeyCode::Unidentified(4242));
    }

    /// R02-10：滚轮是行单位 + 离散阶段；触控板是像素单位 + 连续阶段。
    #[test]
    fn scroll_events_carry_the_unit_and_the_phase() {
        let wheel = scroll_event(
            Point::ZERO,
            (0.0, -1.0),
            false,
            ScrollPhase::Discrete,
            Modifiers::empty(),
        );
        let finger = scroll_event(
            Point::ZERO,
            (0.0, -10.0),
            true,
            ScrollPhase::Update,
            Modifiers::empty(),
        );

        let crate::event::InputEvent::Scroll {
            delta, unit, phase, ..
        } = wheel
        else {
            panic!("必须是滚动事件");
        };
        assert_eq!(delta, Point::new(0.0, -1.0));
        assert_eq!(unit, ScrollUnit::Line);
        assert_eq!(phase, ScrollPhase::Discrete);

        let crate::event::InputEvent::Scroll { unit, phase, .. } = finger else {
            panic!("必须是滚动事件");
        };
        assert_eq!(unit, ScrollUnit::Pixel);
        assert_eq!(phase, ScrollPhase::Update);
    }

    /// R02-2（P0 回归，Linux）：合成重复必须继续产生字符文本。
    ///
    /// Wayland 的按键重复由**客户端**合成（服务端从不重发 `key` 事件），字符文本也只能由
    /// 客户端按每次重复生成；而默认通路是键盘回退（`TextInputRoute::takes_text() == false`，
    /// 即 IME 未接管或无输入法守护进程时）。旧实现把
    /// `!repeat` 写进文本条件 ⇒ 按住可打印键只输入一个字符。
    ///
    /// 判据在 `crate::event::should_emit_text`（平台无关、有独立单测），这里钉住 Linux 侧的
    /// 参数组合：XKB 回退通路（IME 未接管、非组合期）在 `repeat = true` 时仍必须产生文本。
    #[test]
    fn repeated_key_down_still_emits_text_on_the_xkb_fallback() {
        use crate::event::{KeyState, should_emit_text};

        assert!(should_emit_text(KeyState::Down, false, false, false));
        assert!(
            should_emit_text(KeyState::Down, true, false, false),
            "合成重复必须继续产生文本（P0 回归）"
        );
        // IME 接管后 XKB 通路必须静默（文本只走 text-input-v3 的 commit_string）。
        assert!(!should_emit_text(KeyState::Down, true, true, false));
    }

    /// R02-1：媒体键在 Linux 上也映射到与 Windows `E0` 扩展行**相同**的物理身份。
    #[test]
    fn media_keys_map_to_the_same_identities_as_windows() {
        assert_eq!(key_code(115), KeyCode::AudioVolumeUp);
        assert_eq!(key_code(114), KeyCode::AudioVolumeDown);
        assert_eq!(key_code(113), KeyCode::AudioVolumeMute);
        assert_eq!(key_code(164), KeyCode::MediaPlayPause);
        assert_eq!(key_code(163), KeyCode::MediaTrackNext);
        assert_eq!(key_code(165), KeyCode::MediaTrackPrevious);
        assert_eq!(key_code(166), KeyCode::MediaStop);
        assert_eq!(key_code(172), KeyCode::BrowserHome);
        assert_eq!(key_code(158), KeyCode::BrowserBack);
        assert_eq!(key_code(159), KeyCode::BrowserForward);
    }

    /// `ScrollAccum` 的像素通路（触控板 1:1 跟随）与行通路互相独立。
    #[test]
    fn continuous_pixels_are_not_converted_to_lines() {
        let touchpad = ScrollAccum {
            continuous_y: -24.0,
            ..ScrollAccum::default()
        };
        assert_eq!(touchpad.continuous_pixels().1, -24.0);
        assert_eq!(touchpad.lines().1, -2.4);
    }
}

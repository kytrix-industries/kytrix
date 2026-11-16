//! Wayland surface / 协议状态机（`design/phase-12-macos-linux.md` §8.2 / §8.8）。
//!
//! 本文件是 **唯一的协议事件翻译点**：所有 `wl_*` / `xdg_*` / `wp_*` 事件在这里变成公共
//! `Event`（与 Win32 的 `translate` 同层职责），泵在主循环里 drain 后交给 `App::dispatch`。
//!
//! 约束（D25 / I16）：只走 Wayland；缺 `xdg_wm_base` / `wl_compositor` / `wl_seat` 一律
//! `EngineError::WaylandCompositor`，**不回退 X11**。
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
        wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
        wl_data_source, wl_keyboard, wl_keyboard as wl_kb, wl_output, wl_pointer, wl_registry,
        wl_seat, wl_surface,
    },
};
use wayland_protocols::{
    wp::{
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
    error::EngineError,
    event::{Event, ImeEvent, InputEvent, Key, KeyState, Modifiers, PointerButton},
    math::{Point, Rect, Scale, Size},
    platform::linux::{
        clipboard::Clipboard,
        dpi,
        frame_gate::{CallbackToken, FrameGate},
        ime::ImeState,
        window,
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
    /// 最近一次 configure 的 `Maximized` 状态（自绘标题栏的切换按钮据此选择 set/unset）。
    maximized: bool,
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
    text_input: Option<ZwpTextInputV3>,
    /// 已绑定的 `wl_output`（容量 2：常见单 / 双屏；多屏时扩容，O(n) 扫描无所谓）。
    outputs: Vec<OutputEntry>,
    ime: ImeState,
    clipboard: Clipboard,
    xkb_context: xkb::Context,
    xkb_state: Option<xkb::State>,
    /// 最近的输入 serial（IME / 剪贴板 / 拖拽都要）。
    serial: u32,
    /// 最近一次指针按键的 serial（`xdg_toplevel.move` / `resize` 的隐式抓取凭证）。
    pointer_serial: u32,
    /// `wl_keyboard.modifiers` 的原始掩码：`(depressed, latched, locked, group)`。
    ///
    /// `group` 是**当前键盘布局组**。之前把它丢成恒 0，切换布局后字符仍按第一布局解析
    /// （P1-2.11）。xkbcommon 的 `update_mask` 把有效组放在 `locked_layout` 位置。
    mods: (u32, u32, u32, u32),
    modifiers: Modifiers,
    /// `zwp_text_input_v3` 是否已对当前 surface `enable()`（P1-2.11）。
    ///
    /// 一旦启用 IME，**所有**文本都经 `commit_string` 到达（含普通 ASCII）—— 此时必须抑制
    /// `wl_keyboard` 那条通路，否则一次按键会产生两份文本（输入 "ni" 得到 "ni你"）。
    /// 组合器没有 text-input-v3 时保持 `false`，回退到 `wl_keyboard` 取文本。
    ime_enabled: bool,
    /// 键盘重复的参数（`wl_keyboard.repeat_info`；v4+）。Wayland 规定重复由**客户端**合成。
    key_repeat: KeyRepeat,
    /// 当前按下的键（用于合成重复）。顺序即按下顺序，释放时按 keycode 移除。
    held_keys: Vec<HeldKey>,
    /// 本帧尚未提交的滚轮累积（按 `wl_pointer.frame` 成批，P1-2.11）。
    scroll: ScrollAccum,
    pointer_pos: Point,
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
        let connection =
            Connection::connect_to_env().map_err(|error| EngineError::WaylandCompositor {
                detail: smol_str::format_smolstr!("no Wayland session: {error}"),
            })?;
        let (globals, queue) = registry_queue_init::<Self>(&connection).map_err(|error| {
            EngineError::WaylandCompositor {
                detail: smol_str::format_smolstr!("registry: {error}"),
            }
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
            .bind::<ZxdgDecorationManagerV1, _, _>(&queue_handle, 1..=1, ())
            .ok();

        let data_device = data_device_manager.get_data_device(&seat, &queue_handle, ());
        let data_source = data_device_manager.create_data_source(&queue_handle, ());
        for mime in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"] {
            data_source.offer(mime.to_owned());
        }

        let text_input = globals
            .bind::<ZwpTextInputManagerV3, _, _>(&queue_handle, 1..=1, ())
            .ok()
            .map(|manager| manager.get_text_input(&seat, &queue_handle, ()));

        let clipboard = Clipboard::new(connection.clone(), data_device.clone(), data_source);
        let mut state = Self {
            windows: Vec::new(),
            // `windows` 里第一个窗口由下面的 `add_window` 补齐；此前没有事件派发。
            active: 0,
            pointer_focus: None,
            events: Vec::with_capacity(16),
            connection,
            queue_handle,
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
            outputs: Vec::with_capacity(2),
            ime: ImeState::new(),
            clipboard,
            xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
            serial: 0,
            pointer_serial: 0,
            mods: (0, 0, 0, 0),
            modifiers: Modifiers::empty(),
            ime_enabled: false,
            key_repeat: KeyRepeat::default(),
            held_keys: Vec::new(),
            scroll: ScrollAccum::default(),
            pointer_pos: Point::new(0.0, 0.0),
        };
        // 启动时绑定所有现存 `wl_output`：`initial_roundtrip` 的 2 次 roundtrip 会带回它们的
        // `wl_output.scale`，整数回退的首帧因此能按「全部已绑定 output 的最大 scale」兜底
        // （`wl_surface.enter` 要等 surface 映射之后才到，V-110）。
        state.bind_existing_outputs(&globals);
        state.add_window(id, size, title)?;
        Ok((state, queue))
    }

    /// 建一个原生窗口的协议对象：`wl_surface` → `xdg_surface` → `xdg_toplevel` → 装饰 /
    /// fractional / viewport，然后追加进 [`Self::windows`]。
    ///
    /// 启动路径（[`Self::new`] 的首个窗口）与运行期新增窗口共用这一条建窗顺序：协议对象之间
    /// 有严格的创建顺序（toplevel 必须在 surface 首次 commit 之前），任何一处漏掉都会让窗口
    /// 少掉对应能力（装饰 / 分数缩放）。首个 configure 由 compositor 异步发回，事件循环在收到
    /// `configured` 之前不会为它建 swapchain / 出帧。
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
        // 最小尺寸是 surface-local 逻辑像素，与 scale 无关：只需在首个 commit 前设置一次。
        window::apply_min_size(&toplevel);
        surface.commit();

        // 装饰：自绘标题栏，请求 CLIENT_SIDE（没有该扩展时 compositor 自行决定）。
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
            fractional,
            viewport,
            uses_viewport,
            decoration,
            // Wayland 的 configure / window geometry 是整数，小数的逻辑尺寸在协议层没有表示：
            // 这里先四舍五入取整，整数回退下 buffer = `round(L × N)` 因而严格是 buffer_scale 的
            // 整数倍（否则 `wl_surface.attach` 报 `invalid_size`，V-110）。
            size: Size::new(size.width.round().max(1.0), size.height.round().max(1.0)),
            scale: Scale::ONE,
            surface_outputs: Vec::with_capacity(2),
            preferred_buffer_scale: None,
            fractional_scale_120: None,
            integer_scale: 1,
            entered_once: false,
            focused: false,
            configured: false,
            maximized: false,
            close_requested: false,
            frame_gate: FrameGate::new(),
            frame_callback: None,
        });
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
        self.windows.remove(index);
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
    pub(crate) fn minimize(&self, index: usize) {
        self.windows[index].toplevel.set_minimized();
    }

    /// 最大化 / 还原切换；最终状态以 compositor 的 configure 为准。
    pub(crate) fn toggle_maximize(&mut self, index: usize) {
        if self.windows[index].maximized {
            self.windows[index].toplevel.unset_maximized();
        } else {
            self.windows[index].toplevel.set_maximized();
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

    /// 活性兜底：任一窗口的门禁关闭超过 `GATE_TIMEOUT` 时强制打开（返回 `true` = 有窗口被兜底）。
    /// 只覆盖「publish 了但没 commit」（present `OUT_OF_DATE` 等），正常路径不会触发；
    /// **不是**呈现节奏（F-183）。
    pub(crate) fn expire_frame_gates(&mut self, now: Instant) -> bool {
        let mut expired = false;
        for window in &mut self.windows {
            expired |= window.frame_gate.expire(now);
        }
        expired
    }

    /// 所有窗口里最早的活性兜底期限（门禁全开时 `None`）；事件循环用它压缩阻塞等待。
    pub(crate) fn frame_gate_remaining(&self, now: Instant) -> Option<Duration> {
        self.windows
            .iter()
            .filter_map(|window| window.frame_gate.time_until_expiry(now))
            .min()
    }

    /// IME 的激活/停用（focus 变化时由键盘事件驱动）。
    fn set_ime_focus(&mut self, index: usize, focused: bool) {
        let Some(text_input) = self.text_input.as_ref() else {
            return;
        };
        if focused {
            // v3：enable 会重置此前 set_cursor_rectangle 的状态；必须先 enable，再设矩形，再 commit。
            // 绑定 version 1：状态在 `zwp_text_input_v3.commit` 生效（v2+ 的 cursor rectangle 才需要
            // surface 的 `wl_surface.commit`）。**不** commit `wl_surface`：UI 线程的 commit 会提前
            // 触发 frame callback、破坏 FrameGate 的「≤1 在飞」（D4）。
            text_input.enable();
            // P1-2.11：从这里开始文本由 `commit_string` 送达，`wl_keyboard` 通路必须静默。
            self.ime_enabled = true;
            let area = self.windows[index]
                .ime_cursor_area
                .unwrap_or_else(|| Rect::new(0.0, 0.0, 1.0, 1.0));
            text_input.set_cursor_rectangle(
                area.origin.x.round() as i32,
                area.origin.y.round() as i32,
                area.size.width.max(1.0).round() as i32,
                area.size.height.max(1.0).round() as i32,
            );
            self.windows[index].ime_cursor_applied = Some(area);
        } else {
            text_input.disable();
            self.ime_enabled = false;
            self.windows[index].ime_cursor_applied = None;
        }
        text_input.commit();
    }

    /// 保存 IME caret 区域；已聚焦时立即下发（`commit` 是 v3 的生效点）。
    fn set_ime_cursor_area(&mut self, index: usize, area: Option<Rect>) {
        if self.windows[index].ime_cursor_area == area {
            return;
        }
        self.windows[index].ime_cursor_area = area;
        if !self.windows[index].focused {
            return;
        }
        let effective = area.unwrap_or_else(|| Rect::new(0.0, 0.0, 1.0, 1.0));
        if let Some(text_input) = self.text_input.as_ref() {
            if self.windows[index].ime_cursor_applied != Some(effective) {
                text_input.set_cursor_rectangle(
                    effective.origin.x.round() as i32,
                    effective.origin.y.round() as i32,
                    effective.size.width.max(1.0).round() as i32,
                    effective.size.height.max(1.0).round() as i32,
                );
                self.windows[index].ime_cursor_applied = Some(effective);
            }
            text_input.commit();
        }
    }

    /// 从 `App` 同步指定窗口的 IME caret 区域（每轮泵逐窗口调用，独立于帧门禁）。
    pub(crate) fn sync_ime_cursor_area(&mut self, app: &App, window: crate::window::WindowId) {
        let Some(index) = self.window_index(window) else {
            return;
        };
        let area = match app.ime_cursor_area(window) {
            Ok(area) => area,
            // 文本尚未 shape / 文本刚变化：保留上一次有效区域。
            Err(_) => return,
        };
        self.set_ime_cursor_area(index, area);
    }

    /// 键盘：`wl_keyboard::Event::Key` → `Event`（字符输入 + 非字符键）。
    fn translate_key(&mut self, key: u32, state: KeyState) {
        let Some(xkb_state) = self.xkb_state.as_mut() else {
            return;
        };
        let (depressed, latched, locked, group) = self.mods;
        // 有效布局组放在 `locked_layout` 位置（Wayland 只给有效组，smithay/winit 同此约定）。
        xkb_state.update_mask(depressed, latched, locked, 0, 0, group);
        let keycode = xkb::Keycode::new(key);
        let symbol = xkb_state.key_get_one_sym(keycode);
        // seat 事件没有 surface 归属：派发给**焦点窗口**（ctive）。
        let window = self.windows[self.active].id;
        self.events.push((
            window,
            Event::Input(InputEvent::Key {
                key: map_keysym(symbol, key),
                state,
                modifiers: self.modifiers,
            }),
        ));
        // P1-2.11：IME 启用后文本**只**走 `zwp_text_input_v3` 的 `commit_string`。
        // 两条通路同时生效会让一次按键产生两份文本（输入 "ni" 得到 "ni你"）。
        // 组合器没有 text-input-v3 时 `ime_enabled` 恒 false，回退到这里的 xkb 通路。
        if state == KeyState::Down && !self.ime_enabled {
            let text = xkb_state.key_get_utf8(keycode);
            if !text.is_empty() && text.chars().all(|c| !c.is_control()) {
                self.events.push((
                    window,
                    Event::Input(InputEvent::Text {
                        text: SmolStr::new(text),
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
            self.translate_key(key, KeyState::Down);
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
            .map_err(|error| EngineError::WaylandCompositor {
                detail: smol_str::format_smolstr!("dispatch_pending: {error}"),
            })
    }
}

/// 绑定一个必需 global（缺失 / 版本不足 ⇒ `WaylandCompositor`）。
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
        .map_err(|error| EngineError::WaylandCompositor {
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
    WpFractionalScaleManagerV1,
    WpViewporter,
    WpViewport,
    ZwpTextInputManagerV3,
    ZxdgDecorationManagerV1,
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
            }
            wl_surface::Event::Leave { output } => {
                let id = output.id();
                state.windows[index]
                    .surface_outputs
                    .retain(|entered| *entered != id);
                state.recompute_scale(index);
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
                // 0 = 「客户端自己选」；否则用 compositor 给的建议尺寸。
                if width > 0 && height > 0 {
                    state.windows[index].size = Size::new(width as f32, height as f32);
                }
                // `states` 是协议数组的原始字节（u32 native-endian 的 state 枚举序列）。
                let maximized = xdg_toplevel::State::Maximized as u32;
                state.windows[index].maximized = states
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|chunk| u32::from_ne_bytes(*chunk) == maximized);
                state.windows[index].configured = true;
                let size = state.windows[index].size;
                let window = state.windows[index].id;
                state.events.push((window, Event::Resized { size }));
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
                        state.xkb_state = Some(xkb::State::new(&keymap));
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
                    state.translate_key(xkb_key, key_state);
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
                state.set_ime_focus(index, true);
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.windows[index].focused = false;
                let window = state.windows[index].id;
                state
                    .events
                    .push((window, Event::FocusChanged { focused: false }));
                state.set_ime_focus(index, false);
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
                state.pointer_pos = Point::new(surface_x as f32, surface_y as f32);
                let window = state.windows[index].id;
                state.events.push((
                    window,
                    Event::Input(InputEvent::PointerMove {
                        position: state.pointer_pos,
                        modifiers: state.modifiers,
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
                    let event = match button_state {
                        wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed) => {
                            state.pointer_serial = serial;
                            InputEvent::PointerDown {
                                position: state.pointer_pos,
                                button,
                                modifiers: state.modifiers,
                            }
                        }
                        _ => InputEvent::PointerUp {
                            position: state.pointer_pos,
                            button,
                            modifiers: state.modifiers,
                        },
                    };
                    if let Some(window) = state.pointer_window() {
                        state.events.push((window, Event::Input(event)));
                    }
                }
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
            wl_pointer::Event::Frame => {
                // 一次 frame = 一批同属一个逻辑输入的指针事件 ⇒ 只产出一个 Scroll。
                // 未实现 `axis_value120` 的服务端（v8 以下）只会来连续 `axis`，两条通路在此合并。
                if !state.scroll.is_empty() {
                    let (lines_x, lines_y) = state.scroll.lines();
                    state.scroll.clear();
                    // 取反对齐 Win32 的「正 = 向上 / 向右」。
                    if let Some(window) = state.pointer_window() {
                        state.events.push((
                            window,
                            Event::Input(InputEvent::Scroll {
                                position: state.pointer_pos,
                                delta_x: -lines_x,
                                delta_y: -lines_y,
                                modifiers: state.modifiers,
                            }),
                        ));
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                let window = state.pointer_window();
                state.pointer_focus = None;
                if let Some(window) = window {
                    state
                        .events
                        .push((window, Event::Input(InputEvent::PointerLeave)));
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
        state.clipboard.handle_device_event(event);
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
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.clipboard.handle_offer_event(event);
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
            // 对端换了选区属主：我们不再持有剪贴板内容。
            wl_data_source::Event::Cancelled => state.clipboard.handle_cancelled(),
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
        match event {
            zwp_text_input_v3::Event::PreeditString { text, .. } => {
                state.ime.note_preedit(text);
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                state.ime.note_commit(text);
            }
            zwp_text_input_v3::Event::Done { .. } => {
                // IME 只服务于**焦点窗口**（text-input-v3 每窗口一个对象，但 `done` 事件不带
                // 对象信息，因此按 `active` 派发）。
                let window = state.windows[state.active].id;
                for event in state.ime.take_events() {
                    state.events.push((window, Event::Ime(event as ImeEvent)));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        _: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // CONFIGURE 只报告 compositor 的最终选择；引擎始终自绘标题栏（V-86）。
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
            .map_err(|error| EngineError::WaylandCompositor {
                detail: smol_str::format_smolstr!("roundtrip: {error}"),
            })?;
    }
    state.dispatch_pending(queue)?;
    // 启动路径上 `new` 只建了首个窗口（下标 0）；首帧之前它必须拿到 configure（V-110）。
    if !state.is_configured(0) {
        return Err(EngineError::WaylandCompositor {
            detail: smol_str::SmolStr::new_static("no xdg_toplevel.configure before first frame"),
        });
    }
    Ok(())
}

/// 把 `wl_keyboard` / `wl_pointer` 建出来（或按最新的 capabilities 释放）。
///
/// **只有 seat 声明过的能力才能 bind**：`wl_seat.get_keyboard` 在没有 KEYBOARD 能力时是协议错误，
/// 服务端会回 `wl_seat error 0` 并直接断开连接（F-146）。幂等：每轮 capabilities 变化都调用。
pub(crate) fn create_devices(state: &mut WaylandState) -> Result<(), EngineError> {
    let queue = state.queue_handle.clone();
    let caps = state.seat_caps;
    if caps.contains(wl_seat::Capability::Keyboard) {
        if state.keyboard.is_none() {
            state.keyboard = Some(state.seat.get_keyboard(&queue, ()));
        }
    } else {
        state.keyboard = None;
    }
    if caps.contains(wl_seat::Capability::Pointer) {
        if state.pointer.is_none() {
            state.pointer = Some(state.seat.get_pointer(&queue, ()));
        }
    } else {
        state.pointer = None;
    }
    Ok(())
}

/// 空闲等待的兜底超时（`ping` / 协议事件会提前返回；3600 s 只是防御）。
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);

/// `Keysym → Key` 映射（未映射 ⇒ `Key::Other(evdev)`）。
fn map_keysym(symbol: xkb::Keysym, evdev: u32) -> Key {
    let code = u32::from(symbol);
    match code {
        xkb::keysyms::KEY_Escape => Key::Escape,
        xkb::keysyms::KEY_Return | xkb::keysyms::KEY_KP_Enter => Key::Enter,
        xkb::keysyms::KEY_Tab => Key::Tab,
        xkb::keysyms::KEY_BackSpace => Key::Backspace,
        xkb::keysyms::KEY_Delete => Key::Delete,
        xkb::keysyms::KEY_space => Key::Space,
        xkb::keysyms::KEY_Up => Key::ArrowUp,
        xkb::keysyms::KEY_Down => Key::ArrowDown,
        xkb::keysyms::KEY_Left => Key::ArrowLeft,
        xkb::keysyms::KEY_Right => Key::ArrowRight,
        xkb::keysyms::KEY_Home => Key::Home,
        xkb::keysyms::KEY_End => Key::End,
        xkb::keysyms::KEY_Prior => Key::PageUp,
        xkb::keysyms::KEY_Next => Key::PageDown,
        xkb::keysyms::KEY_Shift_L | xkb::keysyms::KEY_Shift_R => Key::Shift,
        xkb::keysyms::KEY_Control_L | xkb::keysyms::KEY_Control_R => Key::Control,
        xkb::keysyms::KEY_Alt_L | xkb::keysyms::KEY_Alt_R => Key::Alt,
        xkb::keysyms::KEY_Super_L | xkb::keysyms::KEY_Super_R => Key::Meta,
        xkb::keysyms::KEY_F1 => Key::Function(1),
        xkb::keysyms::KEY_F2 => Key::Function(2),
        xkb::keysyms::KEY_F3 => Key::Function(3),
        xkb::keysyms::KEY_F4 => Key::Function(4),
        xkb::keysyms::KEY_F5 => Key::Function(5),
        xkb::keysyms::KEY_F6 => Key::Function(6),
        xkb::keysyms::KEY_F7 => Key::Function(7),
        xkb::keysyms::KEY_F8 => Key::Function(8),
        xkb::keysyms::KEY_F9 => Key::Function(9),
        xkb::keysyms::KEY_F10 => Key::Function(10),
        xkb::keysyms::KEY_F11 => Key::Function(11),
        xkb::keysyms::KEY_F12 => Key::Function(12),
        _ => Key::Other((evdev.saturating_sub(8)) as u16),
    }
}

#[cfg(test)]
mod tests {
    use super::{CONTINUOUS_AXIS_UNITS_PER_LINE, KeyRepeat, ScrollAccum, choose_integer_scale};

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
}

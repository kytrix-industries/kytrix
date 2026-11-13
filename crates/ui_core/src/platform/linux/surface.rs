//! Wayland surface / 协议状态机（`design/phase-12-macos-linux.md` §8.2 / §8.8）。
//!
//! 本文件是 **唯一的协议事件翻译点**：所有 `wl_*` / `xdg_*` / `wp_*` 事件在这里变成公共
//! `Event`（与 Win32 的 `translate` 同层职责），泵在主循环里 drain 后交给 `App::dispatch`。
//!
//! 约束（D25 / I16）：只走 Wayland；缺 `xdg_wm_base` / `wl_compositor` / `wl_seat` 一律
//! `EngineError::WaylandCompositor`，**不回退 X11**。

use std::time::Duration;

use smol_str::SmolStr;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, event_created_child,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
        wl_data_source, wl_keyboard, wl_keyboard as wl_kb, wl_output, wl_pointer, wl_registry,
        wl_seat, wl_surface,
    },
};
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1},
        text_input::zv3::client::{
            zwp_text_input_manager_v3::ZwpTextInputManagerV3,
            zwp_text_input_v3::{self, ZwpTextInputV3},
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::{
        decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1},
        shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
    },
};
use xkbcommon::xkb;

use crate::{
    app::App,
    error::EngineError,
    event::{Event, ImeEvent, InputEvent, Key, KeyState, Modifiers, PointerButton},
    math::{Point, Rect, Scale, Size},
    platform::linux::{a11y::UnixA11y, clipboard::Clipboard, dpi, ime::ImeState, window},
};

/// 客户端自报的 app id（compositor 用来匹配桌面文件）。
const APP_ID: &str = "kytrix";

/// Wayland 侧的全部状态（`Dispatch` 的实现体 + calloop 的 `Data`）。
pub(crate) struct WaylandState {
    /// 待泵消费的平台事件。
    events: Vec<Event>,
    connection: Connection,
    queue_handle: QueueHandle<Self>,
    /// 只为保活（建 surface 之后不再使用）。
    _compositor: wl_compositor::WlCompositor,
    surface: wl_surface::WlSurface,
    /// 只为保活（configure 的 ack 在 handler 里）。
    _xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    /// 只为保活（`xdg_wm_base::ping` 必须回 pong）。
    _wm_base: xdg_wm_base::XdgWmBase,
    seat: wl_seat::WlSeat,
    /// `wl_seat.capabilities` 最近一次声明的能力位：只有声明过的设备才能 `get_keyboard` / `get_pointer`
    /// （否则服务端报 `wl_seat error 0` 并断开连接，见 F-146）。
    seat_caps: wl_seat::Capability,
    keyboard: Option<wl_kb::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    /// 只为保活（`wl_data_device` 的 offer 事件在它上面）。
    _data_device: wl_data_device::WlDataDevice,
    text_input: Option<ZwpTextInputV3>,
    /// 应用声明的 IME caret 区域（surface 逻辑坐标；`None` = 无 hint）。
    ime_cursor_area: Option<Rect>,
    /// 最近一次实际下发给 `zwp_text_input_v3` 的区域（含 None 回退）。
    ime_cursor_applied: Option<Rect>,
    /// 分数缩放（v1 只读 `preferred_scale`；对象必须保活）。
    _fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    /// 分数缩放的落地窗口（`set_destination` 让 compositor 做非整数缩放，D-5）。
    viewport: Option<WpViewport>,
    /// 只为保活（`configure` 事件只做记录）。
    _decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    ime: ImeState,
    clipboard: Clipboard,
    xkb_context: xkb::Context,
    xkb_state: Option<xkb::State>,
    /// 最近的输入 serial（IME / 剪贴板 / 拖拽都要）。
    serial: u32,
    mods: (u32, u32, u32),
    modifiers: Modifiers,
    pointer_pos: Point,
    size: Size,
    scale: Scale,
    /// 最近一次 `size/scale` 是否已经下发到 viewport（避免每轮 commit）。
    viewport_applied: Option<(u32, u32)>,
    focused: bool,
    configured: bool,
    close_requested: bool,
    /// 帧节流：上一次请求的 `wl_callback` 回来之前不再提交。
    frame_ready: bool,
}

impl WaylandState {
    /// 连上 compositor、绑定必需 global、建 surface/toplevel、装 IME / 剪贴板（§8.1 的 1–4）。
    pub(crate) fn new() -> Result<(Self, wayland_client::EventQueue<Self>), EngineError> {
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

        let surface = compositor.create_surface(&queue_handle, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &queue_handle, ());
        let toplevel = xdg_surface.get_toplevel(&queue_handle, ());
        toplevel.set_app_id(APP_ID.to_owned());
        toplevel.set_title("Kytrix".to_owned());
        window::apply_min_size(&toplevel, Scale::ONE);
        surface.commit();

        let data_device = data_device_manager.get_data_device(&seat, &queue_handle, ());
        let data_source = data_device_manager.create_data_source(&queue_handle, ());
        for mime in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"] {
            data_source.offer(mime.to_owned());
        }

        let text_input = globals
            .bind::<ZwpTextInputManagerV3, _, _>(&queue_handle, 1..=1, ())
            .ok()
            .map(|manager| manager.get_text_input(&seat, &queue_handle, ()));
        let fractional = globals
            .bind::<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, _, _>(
                &queue_handle,
                1..=1,
                (),
            )
            .ok()
            .map(|manager| manager.get_fractional_scale(&surface, &queue_handle, ()));
        let decoration = globals
            .bind::<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, _, _>(
                &queue_handle,
                1..=1,
                (),
            )
            .ok()
            .map(|manager| {
                let decoration = manager.get_toplevel_decoration(&toplevel, &queue_handle, ());
                // 自绘标题栏：请求 CLIENT_SIDE（没有该扩展时 compositor 自行决定）。
                decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide);
                decoration
            });

        let viewport = globals
            .bind::<WpViewporter, _, _>(&queue_handle, 1..=1, ())
            .ok()
            .map(|viewporter| viewporter.get_viewport(&surface, &queue_handle, ()));

        let clipboard = Clipboard::new(connection.clone(), data_device.clone(), data_source);
        let state = Self {
            events: Vec::with_capacity(16),
            connection,
            queue_handle,
            _compositor: compositor,
            surface,
            _xdg_surface: xdg_surface,
            toplevel,
            _wm_base: wm_base,
            seat,
            seat_caps: wl_seat::Capability::empty(),
            keyboard: None,
            pointer: None,
            _data_device: data_device,
            text_input,
            ime_cursor_area: None,
            ime_cursor_applied: None,
            _fractional: fractional,
            viewport,
            _decoration: decoration,
            ime: ImeState::new(),
            clipboard,
            xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
            serial: 0,
            mods: (0, 0, 0),
            modifiers: Modifiers::empty(),
            pointer_pos: Point::new(0.0, 0.0),
            size: Size::new(800.0, 600.0),
            scale: Scale::ONE,
            viewport_applied: None,
            focused: false,
            configured: false,
            close_requested: false,
            frame_ready: true,
        };
        Ok((state, queue))
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    pub(crate) fn toplevel(&self) -> &xdg_toplevel::XdgToplevel {
        &self.toplevel
    }

    /// `wl_display*` / `wl_surface*`（Vulkan WSI 需要；`system` backend 的真实对象）。
    pub(crate) fn raw_pointers(&self) -> (*mut std::ffi::c_void, *mut std::ffi::c_void) {
        (
            self.connection.display().id().as_ptr().cast(),
            self.surface.id().as_ptr().cast(),
        )
    }

    pub(crate) fn size(&self) -> Size {
        self.size
    }

    pub(crate) fn scale(&self) -> Scale {
        self.scale
    }

    pub(crate) fn serial(&self) -> u32 {
        self.serial
    }

    pub(crate) fn is_configured(&self) -> bool {
        self.configured
    }

    pub(crate) fn is_focused(&self) -> bool {
        self.focused
    }

    /// 尺寸 / 缩放变化后把逻辑尺寸交给 `wp_viewport.set_destination`（D-5）。
    ///
    /// 没有 viewporter（compositor 不支持）⇒ 什么都不做：此时只能靠整数倍率 + buffer scale。
    pub(crate) fn sync_viewport(&mut self) {
        let Some(viewport) = self.viewport.as_ref() else {
            return;
        };
        let (width, height) = dpi::viewport_destination(self.size);
        let key = (width as u32, height as u32);
        if self.viewport_applied == Some(key) {
            return;
        }
        self.viewport_applied = Some(key);
        viewport.set_destination(width, height);
        // 有 viewport 时 `buffer_scale` **必须**是 1：缩放由 destination 完成，
        // 再设整数倍率会变成双重缩放（buffer 尺寸本身已经是物理像素）。
        self.surface.set_buffer_scale(1);
        self.surface.commit();
    }

    pub(crate) fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn take_close_request(&mut self) -> bool {
        std::mem::take(&mut self.close_requested)
    }

    pub(crate) fn clipboard_mut(&mut self) -> &mut Clipboard {
        &mut self.clipboard
    }

    /// 请求一帧回调（提交之后调用；`frame_ready == false` 期间不再提交）。
    pub(crate) fn request_frame(&mut self) {
        self.surface.frame(&self.queue_handle, ());
        self.frame_ready = false;
    }

    pub(crate) fn frame_ready(&self) -> bool {
        self.frame_ready
    }

    /// IME 的激活/停用（focus 变化时由键盘事件驱动）。
    fn set_ime_focus(&mut self, focused: bool) {
        let Some(text_input) = self.text_input.as_ref() else {
            return;
        };
        if focused {
            // v3：enable 会重置此前 set_cursor_rectangle 的状态；必须先 enable，再设矩形，再 commit。
            // 当前绑定 version 1；若未来绑 v2+，cursor rectangle 还需 surface 的 wl_surface.commit。
            text_input.enable();
            let area = self
                .ime_cursor_area
                .unwrap_or_else(|| Rect::new(0.0, 0.0, 1.0, 1.0));
            text_input.set_cursor_rectangle(
                area.origin.x.round() as i32,
                area.origin.y.round() as i32,
                area.size.width.max(1.0).round() as i32,
                area.size.height.max(1.0).round() as i32,
            );
            self.ime_cursor_applied = Some(area);
        } else {
            text_input.disable();
            self.ime_cursor_applied = None;
        }
        text_input.commit();
        self.surface.commit();
    }

    /// 保存 IME caret 区域；已聚焦时立即下发（`commit` 是 v3 的生效点）。
    pub(crate) fn set_ime_cursor_area(&mut self, area: Option<Rect>) {
        if self.ime_cursor_area == area {
            return;
        }
        self.ime_cursor_area = area;
        if !self.focused {
            return;
        }
        let effective = area.unwrap_or_else(|| Rect::new(0.0, 0.0, 1.0, 1.0));
        if let Some(text_input) = self.text_input.as_ref() {
            if self.ime_cursor_applied != Some(effective) {
                text_input.set_cursor_rectangle(
                    effective.origin.x.round() as i32,
                    effective.origin.y.round() as i32,
                    effective.size.width.max(1.0).round() as i32,
                    effective.size.height.max(1.0).round() as i32,
                );
                self.ime_cursor_applied = Some(effective);
            }
            text_input.commit();
        }
        self.surface.commit();
    }

    /// 从 `App` 同步当前窗口的 IME caret 区域（每轮泵调用，独立于帧门禁）。
    pub(crate) fn sync_ime_cursor_area(&mut self, app: &App, window: crate::window::WindowId) {
        let area = match app.ime_cursor_area(window) {
            Ok(area) => area,
            // 文本尚未 shape / 文本刚变化：保留上一次有效区域。
            Err(_) => return,
        };
        self.set_ime_cursor_area(area);
    }

    /// 交付一笔 a11y 更新（`accesskit_unix` 的 `update_if_active` 无返回值）。
    pub(crate) fn update_a11y(&mut self, adapter: &mut UnixA11y, app: &mut App) {
        let Some(id) = app.windows().next().map(|(id, _)| id) else {
            return;
        };
        if app.a11y_pending(id) {
            adapter.update(|| crate::a11y::adapter::build_update(app, id));
        }
    }

    /// 键盘：`wl_keyboard::Event::Key` → `Event`（字符输入 + 非字符键）。
    fn translate_key(&mut self, key: u32, state: KeyState) {
        let Some(xkb_state) = self.xkb_state.as_mut() else {
            return;
        };
        let (depressed, latched, locked) = self.mods;
        xkb_state.update_mask(depressed, latched, locked, 0, 0, 0);
        let keycode = xkb::Keycode::new(key);
        let symbol = xkb_state.key_get_one_sym(keycode);
        self.events.push(Event::Input(InputEvent::Key {
            key: map_keysym(symbol, key),
            state,
            modifiers: self.modifiers,
        }));
        if state == KeyState::Down {
            let text = xkb_state.key_get_utf8(keycode);
            if !text.is_empty() && text.chars().all(|c| !c.is_control()) {
                self.events.push(Event::Input(InputEvent::Text {
                    text: SmolStr::new(text),
                }));
            }
        }
    }

    fn update_modifiers(&mut self) {
        self.modifiers = Modifiers::empty();
        let (depressed, latched, locked) = self.mods;
        let Some(state) = self.xkb_state.as_mut() else {
            return;
        };
        state.update_mask(depressed, latched, locked, 0, 0, 0);
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
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
    wl_output::WlOutput,
    wl_data_device_manager::WlDataDeviceManager,
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    WpViewporter,
    WpViewport,
    ZwpTextInputManagerV3,
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
);

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
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 输出变化不影响尺寸（分数缩放走 `wp_fractional_scale`，整数回退走 configure）。
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_ready = true;
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
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                // 0 = 「客户端自己选」；否则用 compositor 给的建议尺寸。
                if width > 0 && height > 0 {
                    state.size = Size::new(width as f32, height as f32);
                }
                state.configured = true;
                state.events.push(Event::Resized { size: state.size });
            }
            xdg_toplevel::Event::Close => {
                state.close_requested = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Ok(next) = dpi::scale_from_120(scale)
        {
            state.scale = next;
            state.events.push(Event::DpiChanged {
                dpi: (next.factor() * 96.0).round() as u32,
                scale: next,
            });
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
                ..
            } => {
                state.mods = (mods_depressed, mods_latched, mods_locked);
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
                    state.translate_key(key + 8, key_state);
                }
            }
            wl_keyboard::Event::Enter { serial, .. } => {
                state.serial = serial;
                state.focused = true;
                state.events.push(Event::FocusChanged { focused: true });
                state.set_ime_focus(true);
            }
            wl_keyboard::Event::Leave { .. } => {
                state.focused = false;
                state.events.push(Event::FocusChanged { focused: false });
                state.set_ime_focus(false);
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
                surface_x,
                surface_y,
                ..
            } => {
                state.serial = serial;
                state.pointer_pos = Point::new(surface_x as f32, surface_y as f32);
                state.events.push(Event::Input(InputEvent::PointerMove {
                    position: state.pointer_pos,
                    modifiers: state.modifiers,
                }));
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_pos = Point::new(surface_x as f32, surface_y as f32);
                state.events.push(Event::Input(InputEvent::PointerMove {
                    position: state.pointer_pos,
                    modifiers: state.modifiers,
                }));
            }
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => {
                if let Some(button) = Self::pointer_button(button) {
                    let event = match button_state {
                        wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed) => {
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
                    state.events.push(Event::Input(event));
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                // `value` 是 1/120 行；取反对齐 Win32 的「正 = 向上 / 向右」。
                let delta = -(value as f32) / 120.0;
                let (delta_x, delta_y) = match axis {
                    wayland_client::WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                        (delta, 0.0)
                    }
                    _ => (0.0, delta),
                };
                state.events.push(Event::Input(InputEvent::Scroll {
                    position: state.pointer_pos,
                    delta_x,
                    delta_y,
                    modifiers: state.modifiers,
                }));
            }
            wl_pointer::Event::Leave { .. } => {
                state.events.push(Event::Input(InputEvent::PointerLeave));
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
                for event in state.ime.take_events() {
                    state.events.push(Event::Ime(event as ImeEvent));
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

/// 初始化后一次 roundtrip（拿首个 configure + 分数缩放）。
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
    if !state.is_configured() {
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

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
        presentation_time::client::{wp_presentation, wp_presentation_feedback},
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
        Event, ImeEvent, InputEvent, KeyCode, KeyState, Modifiers, ScrollPhase, ScrollUnit,
        should_emit_text,
    },
    math::{Point, Rect, Scale, Size},
    paint::style::Cursor,
    platform::{
        linux::{
            clipboard::Clipboard,
            cursor::CursorManager,
            dpi,
            presentation::{FeedbackData, Presentation, PresentationHandle},
            window,
        },
        wayland::{
            configure::{AckAction, ConfigureLatch},
            frame_gate::{CallbackToken, FrameGate},
            ime::ImeState,
            input, owner,
            parent::{ParentAction, ParentHints},
            presentation::PresentedOrder,
            scroll::{PointerScroll, ScrollAxis, ScrollEvent, ScrollSource},
            text_input::{self, ImeStateSnapshot, TextInputRoute},
            toplevel::{
                self, DecorationMode, SizeConstraintState, ToplevelStates, Visibility,
                WindowGeometry, WmCapabilities,
            },
        },
    },
    render::present_evidence::{PresentationInbox, PresentationOutcome},
};

const APP_ID: &str = "kytrix";

struct OutputEntry {
    output: wl_output::WlOutput,
    name: u32,
    scale: i32,
    refresh_mhz: i32,
}

fn refresh_hz_from_millihz(refresh_mhz: i32) -> Option<f32> {
    (refresh_mhz > 0).then_some(refresh_mhz as f32 / 1000.0)
}

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

#[derive(Clone, Copy, Debug)]
struct KeyRepeat {
    rate: u32,
    delay: u32,
}

impl Default for KeyRepeat {
    fn default() -> Self {
        Self {
            rate: 25,
            delay: 500,
        }
    }
}

impl KeyRepeat {
    fn interval(self) -> Option<std::time::Duration> {
        (self.rate > 0).then(|| std::time::Duration::from_micros(1_000_000 / u64::from(self.rate)))
    }
}

#[derive(Clone, Copy, Debug)]
struct HeldKey {
    key: u32,
    next: std::time::Instant,
}

struct WindowSurface {
    id: crate::window::WindowId,
    surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    configure: ConfigureLatch,
    ime_cursor_area: Option<Rect>,
    text_target: bool,
    ime_applied: Option<ImeStateSnapshot>,
    ime_content: crate::window::ImeContentType,
    ime: ImeState,
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    viewport: Option<WpViewport>,
    uses_viewport: bool,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    decoration_mode: Option<DecorationMode>,
    size: Size,
    scale: Scale,
    surface_outputs: Vec<ObjectId>,
    preferred_buffer_scale: Option<i32>,
    fractional_scale_120: Option<u32>,
    integer_scale: i32,
    entered_once: bool,
    focused: bool,
    configured: bool,
    mapped: bool,
    states: ToplevelStates,
    wm_caps: WmCapabilities,
    size_constraints: SizeConstraintState,
    geometry: Option<WindowGeometry>,
    cursor: Cursor,
    close_requested: bool,
    frame_gate: FrameGate,
    frame_callback: Option<(ObjectId, CallbackToken)>,
    presented_order: PresentedOrder,
    presentation_inbox: PresentationInbox,
}

impl Drop for WindowSurface {
    fn drop(&mut self) {
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
    windows: Vec<WindowSurface>,
    active: usize,
    pointer_focus: Option<usize>,
    events: Vec<(crate::window::WindowId, Event)>,
    cursor: CursorManager,
    clipboard: Clipboard,
    presentation: Presentation,
    connection: Connection,
    queue_handle: QueueHandle<Self>,
    compositor: wl_compositor::WlCompositor,
    wm_base: xdg_wm_base::XdgWmBase,
    viewporter: Option<WpViewporter>,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    decoration_manager: Option<ZxdgDecorationManagerV1>,
    seat: wl_seat::WlSeat,
    seat_caps: wl_seat::Capability,
    keyboard: Option<wl_kb::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    _data_device: wl_data_device::WlDataDevice,
    text_input: Option<ZwpTextInputV3>,
    text_input_route: TextInputRoute,
    outputs: Vec<OutputEntry>,
    xkb_context: xkb::Context,
    xkb_state: Option<xkb::State>,
    xkb_keymap: Option<xkb::Keymap>,
    serial: u32,
    pointer_serial: u32,
    pointer_enter_serial: Option<u32>,
    mods: (u32, u32, u32, u32),
    modifiers: Modifiers,
    key_repeat: KeyRepeat,
    held_keys: Vec<HeldKey>,
    scroll: PointerScroll,
    parent_hints: ParentHints,
    pointer_pos: Point,
    redraw: bool,
}

impl WaylandState {
    pub(crate) fn new(
        id: crate::window::WindowId,
        options: &crate::window::WindowOptions,
    ) -> Result<(Self, wayland_client::EventQueue<Self>), EngineError> {
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

        let viewporter = globals
            .bind::<WpViewporter, _, _>(&queue_handle, 1..=1, ())
            .ok();
        let fractional_manager = globals
            .bind::<WpFractionalScaleManagerV1, _, _>(&queue_handle, 1..=1, ())
            .ok();
        let decoration_manager = globals
            .bind::<ZxdgDecorationManagerV1, _, _>(&queue_handle, 1..=2, ())
            .ok();
        let cursor_shape_manager = globals
            .bind::<WpCursorShapeManagerV1, _, _>(&queue_handle, 1..=2, ())
            .ok();
        let shm = globals
            .bind::<wl_shm::WlShm, _, _>(&queue_handle, 1..=1, ())
            .ok();

        let data_device = data_device_manager.get_data_device(&seat, &queue_handle, ());

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
            cursor: CursorManager::new(cursor_shape_manager, shm),
            text_input_route: TextInputRoute::default(),
            outputs: Vec::with_capacity(2),
            clipboard,
            presentation: Presentation::new(),
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
            scroll: PointerScroll::default(),
            parent_hints: ParentHints::default(),
            pointer_pos: Point::new(0.0, 0.0),
            redraw: false,
        };
        state.bind_existing_outputs(&globals);
        state.bind_existing_presentation(&globals);
        state.add_window(id, options)?;
        Ok((state, queue))
    }

    pub(crate) fn add_window(
        &mut self,
        id: crate::window::WindowId,
        options: &crate::window::WindowOptions,
    ) -> Result<(), EngineError> {
        let surface = self.compositor.create_surface(&self.queue_handle, ());
        let xdg_surface = self
            .wm_base
            .get_xdg_surface(&surface, &self.queue_handle, ());
        let toplevel = xdg_surface.get_toplevel(&self.queue_handle, ());
        toplevel.set_app_id(APP_ID.to_owned());
        toplevel.set_title(options.title.to_string());
        match owner::resolve_owner(options.owner, |owner| self.window_index(owner)) {
            owner::OwnerResolution::Unparented => {}
            owner::OwnerResolution::Existing(index) => {
                let owner = self.windows[index].id;
                let owner_mapped = self.windows[index].mapped;
                match self.parent_hints.note(id, owner, owner_mapped) {
                    ParentAction::SendNow => {
                        toplevel.set_parent(Some(&self.windows[index].toplevel));
                    }
                    ParentAction::Deferred => {}
                }
            }
            owner::OwnerResolution::Missing(owner) => eprintln!(
                "kytrix: window owner {owner:?} does not exist yet or is already closed; \
                 xdg_toplevel.set_parent skipped (the owned window has no transient hint)"
            ),
        }
        let size = Size::new(
            options.size.width.round().max(1.0),
            options.size.height.round().max(1.0),
        );

        let decoration = self.decoration_manager.as_ref().map(|manager| {
            let decoration = manager.get_toplevel_decoration(&toplevel, &self.queue_handle, ());
            decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide);
            decoration
        });
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
        let minimum = options.min_size.unwrap_or(window::MIN_SIZE_LOGICAL);
        self.windows.push(WindowSurface {
            id,
            surface,
            xdg_surface,
            toplevel,
            configure: ConfigureLatch::new(),
            ime_cursor_area: None,
            text_target: false,
            ime_applied: None,
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
            mapped: false,
            states: ToplevelStates::default(),
            wm_caps: WmCapabilities::ASSUMED,
            size_constraints: SizeConstraintState::new(minimum, options.max_size),
            geometry: None,
            cursor: Cursor::Default,
            close_requested: false,
            frame_gate: FrameGate::new(),
            frame_callback: None,
            presented_order: PresentedOrder::new(),
            presentation_inbox: PresentationInbox::new(),
        });
        self.text_input_route.add_window();
        let index = self.windows.len() - 1;
        self.apply_size_limits(index);
        self.apply_window_geometry(index);
        self.windows[self.windows.len() - 1].surface.commit();
        Ok(())
    }

    fn flush_pending_parent_hints(&mut self, owner: crate::window::WindowId) {
        for child in self.parent_hints.take_children_of(owner) {
            let (Some(child_index), Some(owner_index)) =
                (self.window_index(child), self.window_index(owner))
            else {
                continue;
            };
            let parent = self.windows[owner_index].toplevel.clone();
            self.windows[child_index].toplevel.set_parent(Some(&parent));
        }
    }

    pub(crate) fn remove_window(&mut self, id: crate::window::WindowId) -> bool {
        let Some(index) = self.window_index(id) else {
            return false;
        };
        self.held_keys.clear();
        if self.text_input_route.is_focus(index as u32)
            || self.text_input_route.is_keyboard_focus(index as u32)
        {
            self.ime_disable(false);
        }
        self.parent_hints.forget(id);
        self.windows.remove(index);
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

    pub(crate) fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub(crate) fn window_id(&self, index: usize) -> crate::window::WindowId {
        self.windows[index].id
    }

    pub(crate) fn window_index(&self, id: crate::window::WindowId) -> Option<usize> {
        self.windows.iter().position(|window| window.id == id)
    }

    fn bind_existing_outputs(&mut self, globals: &GlobalList) {
        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                self.bind_output(globals.registry(), global.name, global.version);
            }
        }
    }

    fn bind_existing_presentation(&mut self, globals: &GlobalList) {
        let Some(global) = globals
            .contents()
            .clone_list()
            .into_iter()
            .find(|global| global.interface == "wp_presentation")
        else {
            return;
        };
        self.presentation.bind(
            globals.registry(),
            &self.queue_handle,
            global.name,
            global.version,
        );
    }

    fn bind_output(&mut self, registry: &wl_registry::WlRegistry, name: u32, version: u32) {
        if self.outputs.iter().any(|entry| entry.name == name) {
            return;
        }
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
            refresh_mhz: 0,
        });
    }

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
        self.recompute_all_scales();
    }

    fn recompute_all_scales(&mut self) {
        for index in 0..self.windows.len() {
            self.recompute_scale(index);
        }
    }

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

    pub(crate) fn refresh_hz(&self, index: usize) -> Option<f32> {
        let entered = self
            .outputs
            .iter()
            .filter(|entry| {
                self.windows[index]
                    .surface_outputs
                    .contains(&entry.output.id())
            })
            .filter_map(|entry| refresh_hz_from_millihz(entry.refresh_mhz))
            .reduce(f32::max);
        entered.or_else(|| {
            self.outputs
                .iter()
                .filter_map(|entry| refresh_hz_from_millihz(entry.refresh_mhz))
                .reduce(f32::max)
        })
    }

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

    pub(crate) fn buffer_scale(&self, index: usize) -> i32 {
        if self.windows[index].uses_viewport {
            1
        } else {
            self.windows[index].integer_scale
        }
    }

    pub(crate) fn surface_clone(&self, index: usize) -> wl_surface::WlSurface {
        self.windows[index].surface.clone()
    }

    fn pointer_window(&self) -> Option<crate::window::WindowId> {
        self.pointer_focus.map(|index| self.windows[index].id)
    }

    fn window_index_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        let id = surface.id();
        self.windows
            .iter()
            .position(|window| window.surface.id() == id)
    }

    fn window_index_for_toplevel(&self, toplevel: &xdg_toplevel::XdgToplevel) -> Option<usize> {
        let id = toplevel.id();
        self.windows
            .iter()
            .position(|window| window.toplevel.id() == id)
    }

    fn window_index_for_xdg_surface(&self, xdg_surface: &xdg_surface::XdgSurface) -> Option<usize> {
        let id = xdg_surface.id();
        self.windows
            .iter()
            .position(|window| window.xdg_surface.id() == id)
    }

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

    pub(crate) fn is_visible(&self, index: usize) -> bool {
        let window = &self.windows[index];
        toplevel::is_visible(Visibility {
            configured: window.configured,
            suspended: window.states.suspended,
            entered_once: window.entered_once,
            outputs: window.surface_outputs.len(),
        })
    }

    pub(crate) fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    pub(crate) fn draws_client_decoration(&self, index: usize) -> bool {
        self.windows[index]
            .decoration_mode
            .is_none_or(DecorationMode::draws_client_decoration)
    }

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

    fn apply_size_limits(&mut self, index: usize) {
        let Some(limits) = self.windows[index]
            .size_constraints
            .take_pending(window::SHADOW_MARGIN)
        else {
            return;
        };
        self.windows[index]
            .toplevel
            .set_min_size(limits.min.0, limits.min.1);
        let (max_width, max_height) = limits.max.unwrap_or((0, 0));
        self.windows[index]
            .toplevel
            .set_max_size(max_width, max_height);
    }

    pub(crate) fn take_viewport(&mut self, index: usize) -> Option<WpViewport> {
        self.windows[index].viewport.take()
    }

    pub(crate) fn take_events(&mut self) -> Vec<(crate::window::WindowId, Event)> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn take_close_request(&mut self, index: usize) -> bool {
        std::mem::take(&mut self.windows[index].close_requested)
    }

    pub(crate) fn minimize(&self, index: usize) {
        if !self.windows[index].wm_caps.minimize {
            return;
        }
        self.windows[index].toplevel.set_minimized();
    }

    pub(crate) fn set_title(&self, index: usize, title: &str) {
        self.windows[index].toplevel.set_title(title.to_owned());
    }

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

    pub(crate) fn begin_move(&self, index: usize) {
        self.windows[index]
            .toplevel
            ._move(&self.seat, self.pointer_serial);
    }

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

    pub(crate) fn request_frame(&mut self, index: usize) {
        let Some(token) = self.windows[index].frame_gate.request() else {
            return;
        };
        let callback = self.windows[index].surface.frame(&self.queue_handle, ());
        self.windows[index].frame_callback = Some((callback.id(), token));
    }

    pub(crate) fn frame_ready(&self, index: usize) -> bool {
        self.windows[index].frame_gate.is_ready()
    }

    pub(crate) fn note_frame_not_committed(&mut self, index: usize) {
        self.windows[index].frame_gate.note_not_committed();
    }

    pub(crate) fn presentation_handle(&mut self, index: usize) -> Option<PresentationHandle> {
        let window = self.windows[index].id;
        let surface = self.windows[index].surface.clone();
        self.presentation
            .handle(window, &self.queue_handle, surface)
    }

    pub(crate) fn backfill_presentation(&self, app: &mut App) {
        for native in &self.windows {
            let Some(window) = app.window_mut(native.id) else {
                continue;
            };
            let mut position = 0;
            loop {
                let Some(presentation) = window.recent_frame_presentations().nth(position) else {
                    break;
                };
                position += 1;
                if presentation.presented_at.is_some() {
                    continue;
                }
                let Some(outcome) = native.presentation_inbox.take(presentation.index) else {
                    continue;
                };
                let presented_at = match outcome {
                    PresentationOutcome::Presented(instant) => Some(instant),
                    PresentationOutcome::Discarded => None,
                };
                window.note_frame_presented(presentation.index, presented_at);
            }
        }
    }

    fn note_presentation_terminal(
        &mut self,
        data: &FeedbackData,
        presented: Option<(u32, u32, u32)>,
    ) {
        let Some(index) = self.window_index(data.window()) else {
            return;
        };
        let outcome = match presented {
            Some((tv_sec_hi, tv_sec_lo, tv_nsec)) => {
                if !self.windows[index].presented_order.accept(data.frame()) {
                    self.presentation.note_unmatched_feedback();
                    return;
                }
                let Some(instant) = self
                    .presentation
                    .presented_instant(tv_sec_hi, tv_sec_lo, tv_nsec)
                else {
                    return;
                };
                PresentationOutcome::Presented(instant)
            }
            None => PresentationOutcome::Discarded,
        };
        self.windows[index]
            .presentation_inbox
            .note(data.frame(), outcome);
    }

    pub(crate) fn ack_pending_configure(&mut self, index: usize) -> bool {
        let Some(serial) = self.windows[index].configure.on_frame_published() else {
            return false;
        };
        self.windows[index].xdg_surface.ack_configure(serial);
        true
    }

    fn sync_frame_gate_visibility(&mut self, index: usize) {
        if !self.is_visible(index) {
            self.windows[index].frame_gate.force_idle();
        }
    }

    fn ime_focus_in(&mut self, index: usize) {
        self.text_input_route.focus_in(index as u32);
        self.sync_ime_target(true);
    }

    fn ime_focus_out(&mut self, index: usize) {
        self.text_input_route.focus_out(index as u32);
        self.sync_ime_target(true);
    }

    fn ime_disable(&mut self, emit_end: bool) {
        let previous = self.text_input_route.focus();
        let change = self.text_input_route.disable();
        self.apply_ime_change(previous, change, emit_end);
    }

    fn sync_ime_target(&mut self, emit_end: bool) {
        if self.text_input.is_none() {
            return;
        }
        let previous = self.text_input_route.focus();
        let change = self.text_input_route.reconcile(Instant::now());
        self.apply_ime_change(previous, change, emit_end);
    }

    fn apply_ime_change(
        &mut self,
        previous: Option<u32>,
        change: text_input::FocusChange,
        emit_end: bool,
    ) {
        let Some(text_input) = self.text_input.clone() else {
            return;
        };
        if !change.disable && !change.enable {
            return;
        }
        if change.disable {
            if let Some(index) = previous {
                let ended = self.windows.get_mut(index as usize).is_some_and(|window| {
                    let ended = window.ime.reset_ending();
                    window.ime_applied = None;
                    ended
                });
                if ended
                    && emit_end
                    && let Some(window) = self.windows.get(index as usize)
                {
                    let id = window.id;
                    self.events.push((id, Event::Ime(ImeEvent::End)));
                    self.redraw = true;
                }
            }
            text_input.disable();
        }
        if change.enable
            && let Some(index) = self.text_input_route.focus()
        {
            text_input.enable();
            self.windows[index as usize].ime_applied = None;
            self.push_ime_state(index as usize, &text_input);
        }
        text_input.commit();
        self.text_input_route.note_commit();
    }

    fn push_ime_state(&mut self, index: usize, text_input: &ZwpTextInputV3) -> bool {
        let current = ImeStateSnapshot {
            cursor_area: self.windows[index].ime_cursor_area,
            content: text_input::content_type_bits(self.windows[index].ime_content),
        };
        let Some(diff) = current.diff_from(self.windows[index].ime_applied) else {
            return false;
        };
        if let Some((x, y, width, height)) = diff.cursor_rect {
            text_input.set_cursor_rectangle(x, y, width, height);
        }
        if let Some(bits) = diff.content {
            text_input.set_content_type(
                zwp_text_input_v3::ContentHint::from_bits_truncate(bits.hint),
                zwp_text_input_v3::ContentPurpose::try_from(bits.purpose)
                    .unwrap_or(zwp_text_input_v3::ContentPurpose::Normal),
            );
        }
        self.windows[index].ime_applied = Some(current);
        true
    }

    fn flush_ime_state(&mut self, index: usize, text_input: &ZwpTextInputV3) -> bool {
        if !self.push_ime_state(index, text_input) {
            return false;
        }
        text_input.commit();
        self.text_input_route.note_commit();
        true
    }

    fn note_ime_state(
        &mut self,
        index: usize,
        area: Option<Rect>,
        content: crate::window::ImeContentType,
    ) -> bool {
        let window = &mut self.windows[index];
        let changed = window.ime_content != content || window.ime_cursor_area != area;
        window.ime_content = content;
        window.ime_cursor_area = area;
        changed
    }

    pub(crate) fn sync_ime_policy(&mut self, app: &App) {
        if self.text_input.is_none() {
            return;
        }
        let mut changed: Vec<usize> = Vec::with_capacity(self.windows.len());
        for index in 0..self.windows.len() {
            let id = self.windows[index].id;
            let content = app
                .window(id)
                .map_or(crate::window::ImeContentType::Text, |window| {
                    window.ime_content_type()
                });
            let area = match app.ime_cursor_area(id) {
                Ok(area) => area,
                Err(_) => self.windows[index].ime_cursor_area,
            };
            if self.note_ime_state(index, area, content) {
                changed.push(index);
            }
            self.windows[index].text_target = area.is_some();
            self.text_input_route.set_policy(
                index as u32,
                app.ime_enabled(id),
                self.windows[index].text_target,
            );
        }
        self.sync_ime_target(true);
        if self.text_input_route.commit_in_flight() {
            return;
        }
        let Some(focus) = self.text_input_route.focus() else {
            return;
        };
        if !changed.contains(&(focus as usize)) {
            return;
        }
        let Some(text_input) = self.text_input.clone() else {
            return;
        };
        self.flush_ime_state(focus as usize, &text_input);
    }

    pub(crate) fn degrade_text_input_if_silent(&mut self, now: Instant) -> bool {
        if !self.text_input_route.should_degrade(now) {
            return false;
        }
        if let Some(index) = self.text_input_route.focus()
            && let Some(window) = self.windows.get_mut(index as usize)
        {
            window.ime.reset();
        }
        self.redraw = true;
        true
    }

    pub(crate) fn next_ime_degrade_in(&self, now: Instant) -> Option<Duration> {
        self.text_input_route.next_degrade_in(now)
    }

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

    fn apply_cursor(&mut self, index: usize) {
        let Some(serial) = self.pointer_enter_serial else {
            return;
        };
        let cursor = self.windows[index].cursor;
        let scale = if self.windows[index].uses_viewport {
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

    fn hide_cursor(&mut self) {
        let serial = self.pointer_enter_serial.take();
        let pointer = self.pointer.clone();
        if let Some(serial) = serial {
            self.cursor.hide(pointer.as_ref(), serial);
        }
    }

    fn translate_key(&mut self, key: u32, state: KeyState, repeat: bool, timestamp: Instant) {
        let Some(xkb_state) = self.xkb_state.as_mut() else {
            return;
        };
        let (depressed, latched, locked, group) = self.mods;
        xkb_state.update_mask(depressed, latched, locked, 0, 0, group);
        let keycode = xkb::Keycode::new(key);
        let window = self.windows[self.active].id;
        let composing = self.windows[self.active].ime.composing();
        if input::key_delivered(state, self.modifiers, composing) {
            self.events.push((
                window,
                Event::Input(InputEvent::Key {
                    code: key_code(key.saturating_sub(8)),
                    state,
                    repeat,
                    modifiers: self.modifiers,
                    timestamp,
                }),
            ));
        }
        let ime_takes_text = self.text_input_route.takes_text();
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

    fn key_repeats(&self, xkb_key: u32) -> bool {
        self.xkb_keymap
            .as_ref()
            .is_none_or(|keymap| keymap.key_repeats(xkb::Keycode::new(xkb_key)))
    }

    fn reschedule_held_keys(&mut self) {
        let now = std::time::Instant::now();
        let delay = std::time::Duration::from_millis(u64::from(self.key_repeat.delay));
        for held in &mut self.held_keys {
            held.next = now + delay;
        }
    }

    pub(crate) fn emit_due_key_repeats(&mut self, now: std::time::Instant) {
        if self.held_keys.is_empty() {
            return;
        }
        let Some(interval) = self.key_repeat.interval() else {
            return;
        };
        let mut due: Vec<u32> = Vec::new();
        for held in &mut self.held_keys {
            if held.next <= now {
                due.push(held.key);
                held.next = now + interval;
            }
        }
        for key in due {
            if self.key_repeats(key) {
                self.translate_key(key, KeyState::Down, true, now);
            }
        }
    }

    pub(crate) fn next_key_repeat_in(
        &self,
        now: std::time::Instant,
    ) -> Option<std::time::Duration> {
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

fn classify_backend_error(error: &wayland_client::backend::WaylandError) -> WaylandErrorKind {
    match error {
        wayland_client::backend::WaylandError::Io(io) => WaylandErrorKind::from_io_error(io),
        wayland_client::backend::WaylandError::Protocol(_) => WaylandErrorKind::Protocol,
    }
}

fn classify_global_error(error: &wayland_client::globals::GlobalError) -> WaylandErrorKind {
    match error {
        wayland_client::globals::GlobalError::Backend(backend) => classify_backend_error(backend),
        wayland_client::globals::GlobalError::InvalidId(_) => WaylandErrorKind::Protocol,
    }
}

fn classify_dispatch_error(error: &wayland_client::DispatchError) -> WaylandErrorKind {
    match error {
        wayland_client::DispatchError::BadMessage { .. } => WaylandErrorKind::Protocol,
        wayland_client::DispatchError::Backend(backend) => classify_backend_error(backend),
    }
}

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
        queue: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_output" {
                    state.bind_output(registry, name, version);
                }
                if interface == "wp_presentation" {
                    state.presentation.bind(registry, queue, name, version);
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.unbind_output(name);
                state.presentation.unbind(name);
            }
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

impl Dispatch<wp_presentation::WpPresentation, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.presentation.note_clock_id(clk_id);
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, FeedbackData> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        data: &FeedbackData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                ..
            } => state.note_presentation_terminal(data, Some((tv_sec_hi, tv_sec_lo, tv_nsec))),
            wp_presentation_feedback::Event::Discarded => {
                state.note_presentation_terminal(data, None);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Scale { factor } => {
                if let Some(entry) = state.outputs.iter_mut().find(|entry| entry.name == *name) {
                    entry.scale = factor.max(1);
                }
                state.recompute_all_scales();
            }
            wl_output::Event::Mode { refresh, .. } => {
                if let Some(entry) = state.outputs.iter_mut().find(|entry| entry.name == *name) {
                    entry.refresh_mhz = refresh;
                }
            }
            _ => {}
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
            state.seat_caps = match capabilities {
                wayland_client::WEnum::Value(caps) => caps,
                wayland_client::WEnum::Unknown(_) => state.seat_caps,
            };
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
        let Some(index) = state.window_index_for_surface(surface) else {
            return;
        };
        match event {
            wl_surface::Event::Enter { output } => {
                let id = output.id();
                if !state.windows[index].surface_outputs.contains(&id) {
                    state.windows[index].surface_outputs.push(id);
                }
                state.windows[index].entered_once = true;
                state.recompute_scale(index);
                state.sync_frame_gate_visibility(index);
                if state.is_visible(index) {
                    state.redraw = true;
                }
            }
            wl_surface::Event::Leave { output } => {
                let id = output.id();
                state.windows[index]
                    .surface_outputs
                    .retain(|entered| *entered != id);
                state.recompute_scale(index);
                state.sync_frame_gate_visibility(index);
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
            let Some(index) = state.window_index_for_callback(callback) else {
                return;
            };
            let Some((pending, token)) = state.windows[index].frame_callback.as_ref() else {
                return;
            };
            if pending != &callback.id() {
                return;
            }
            let token = *token;
            state.windows[index].frame_callback = None;
            state.windows[index].frame_gate.on_callback(token);
            if !state.windows[index].mapped {
                state.windows[index].mapped = true;
                let window = state.windows[index].id;
                state.flush_pending_parent_hints(window);
            }
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
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for WaylandState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            let Some(index) = state.window_index_for_xdg_surface(xdg_surface) else {
                return;
            };
            match state.windows[index].configure.on_surface_configure(serial) {
                AckAction::AckNow => xdg_surface.ack_configure(serial),
                AckAction::Defer => {}
            }
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
                let previous_states = state.windows[index].states;
                if width > 0 && height > 0 {
                    state.windows[index].size = Size::new(width as f32, height as f32);
                }
                state.windows[index].states = ToplevelStates::parse(&states);
                let was_configured = state.windows[index].configured;
                state.windows[index].configured = true;
                let size = state.windows[index].size;
                let states_now = state.windows[index].states;
                let window = state.windows[index].id;
                state.windows[index]
                    .configure
                    .on_toplevel_configure(size != previous || states_now != previous_states);
                state.apply_window_geometry(index);
                if size != previous || !was_configured {
                    state.events.push((window, Event::Resized { size }));
                }
                state.sync_frame_gate_visibility(index);
                if state.is_visible(index) {
                    state.redraw = true;
                }
            }
            xdg_toplevel::Event::WmCapabilities { capabilities } => {
                let next = WmCapabilities::parse(&capabilities);
                if state.windows[index].wm_caps != next {
                    state.windows[index].wm_caps = next;
                    state.redraw = true;
                }
            }
            xdg_toplevel::Event::ConfigureBounds { width, height } => {
                state.windows[index].size_constraints.set_bounds(
                    (width > 0 && height > 0).then(|| Size::new(width as f32, height as f32)),
                );
                state.apply_size_limits(index);
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
                    let xkb_key = key + 8;
                    state.note_key_held(xkb_key, key_state);
                    state.translate_key(xkb_key, key_state, false, Instant::now());
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.key_repeat = KeyRepeat {
                    rate: u32::try_from(rate).unwrap_or(0),
                    delay: u32::try_from(delay).unwrap_or(0),
                };
                state.reschedule_held_keys();
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.serial = serial;
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.active = index;
                state.windows[index].focused = true;
                let window = state.windows[index].id;
                state
                    .events
                    .push((window, Event::FocusChanged { focused: true }));
                state.ime_focus_in(index);
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                state.held_keys.clear();
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.windows[index].focused = false;
                let window = state.windows[index].id;
                state
                    .events
                    .push((window, Event::FocusChanged { focused: false }));
                state.ime_focus_out(index);
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
        if let Some(scroll) = pointer_scroll_event(&event) {
            if let Some(output) = state.scroll.apply(scroll)
                && let Some(window) = state.pointer_window()
            {
                state.events.push((
                    window,
                    Event::Input(scroll_event(
                        state.pointer_pos,
                        output.delta,
                        output.finger,
                        output.phase,
                        state.modifiers,
                    )),
                ));
            }
            return;
        }
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                state.serial = serial;
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.pointer_focus = Some(index);
                state.pointer_enter_serial = Some(serial);
                state.pointer_pos = Point::new(surface_x as f32, surface_y as f32);
                let window = state.windows[index].id;
                state.apply_cursor(index);
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
                if let Some(button) = input::pointer_button(button) {
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
            wl_pointer::Event::Leave { .. } => {
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
        state.clipboard.handle_device_event(event, state.serial);
    }

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
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.ime_focus_in(index);
            }
            zwp_text_input_v3::Event::Leave { surface } => {
                let Some(index) = state.window_index_for_surface(&surface) else {
                    return;
                };
                state.ime_focus_out(index);
                state.windows[index].ime.reset();
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                state.text_input_route.note_im_event();
                if let Some(index) = state.text_input_route.focus() {
                    state.windows[index as usize]
                        .ime
                        .note_preedit(text, cursor_begin, cursor_end);
                }
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                state.text_input_route.note_im_event();
                if let Some(index) = state.text_input_route.focus() {
                    state.windows[index as usize].ime.note_commit(text);
                }
            }
            zwp_text_input_v3::Event::DeleteSurroundingText { .. } => {
                state.text_input_route.note_im_event();
            }
            zwp_text_input_v3::Event::Done { serial } => {
                let matches = state.text_input_route.settle(serial);
                let Some(index) = state.text_input_route.focus() else {
                    return;
                };
                let index = index as usize;
                let window = state.windows[index].id;
                for event in state.windows[index].ime.take_events() {
                    state.events.push((window, Event::Ime(event as ImeEvent)));
                }
                if matches && let Some(text_input) = state.text_input.clone() {
                    state.flush_ime_state(index, &text_input);
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
            wayland_client::WEnum::Unknown(value) => {
                DecorationMode::from_protocol(value).unwrap_or(DecorationMode::ClientSide)
            }
        };
        if state.windows[index].decoration_mode != Some(mode) {
            state.windows[index].decoration_mode = Some(mode);
            state.redraw = true;
        }
    }
}

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
    if !state.is_configured(0) {
        return Err(EngineError::Wayland {
            kind: WaylandErrorKind::SurfaceLost,
            detail: smol_str::SmolStr::new_static("no xdg_toplevel.configure before first frame"),
        });
    }
    Ok(())
}

pub(crate) fn create_devices(state: &mut WaylandState) -> Result<(), EngineError> {
    let queue = state.queue_handle.clone();
    let caps = state.seat_caps;
    if caps.contains(wl_seat::Capability::Keyboard) {
        if state.keyboard.is_none() {
            state.keyboard = Some(state.seat.get_keyboard(&queue, ()));
        }
    } else {
        state.held_keys.clear();
        if let Some(keyboard) = state.keyboard.take() {
            state.ime_disable(true);
            if keyboard.version() >= 3 {
                keyboard.release();
            }
        }
    }
    if caps.contains(wl_seat::Capability::Pointer) {
        if state.pointer.is_none() {
            let pointer = state.seat.get_pointer(&queue, ());
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

pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);

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

fn pointer_scroll_event(event: &wl_pointer::Event) -> Option<ScrollEvent> {
    Some(match event {
        wl_pointer::Event::AxisSource { axis_source } => ScrollEvent::Source(match axis_source {
            wayland_client::WEnum::Value(wl_pointer::AxisSource::Finger) => ScrollSource::Finger,
            wayland_client::WEnum::Value(wl_pointer::AxisSource::Continuous) => {
                ScrollSource::Continuous
            }
            wayland_client::WEnum::Value(_) => ScrollSource::Wheel,
            wayland_client::WEnum::Unknown(_) => ScrollSource::Unknown,
        }),
        wl_pointer::Event::Axis { axis, value, .. } => ScrollEvent::Continuous {
            axis: scroll_axis(axis),
            value: *value as f32,
        },
        wl_pointer::Event::AxisDiscrete { axis, discrete } => ScrollEvent::Discrete {
            axis: scroll_axis(axis),
            notches: *discrete,
        },
        wl_pointer::Event::AxisValue120 { axis, value120 } => ScrollEvent::Value120 {
            axis: scroll_axis(axis),
            value120: *value120,
        },
        wl_pointer::Event::AxisStop { axis, .. } => ScrollEvent::Stop {
            axis: scroll_axis(axis),
        },
        wl_pointer::Event::Frame => ScrollEvent::Frame,
        _ => return None,
    })
}

fn scroll_axis(axis: &wayland_client::WEnum<wl_pointer::Axis>) -> ScrollAxis {
    if matches!(
        axis,
        wayland_client::WEnum::Value(wl_pointer::Axis::HorizontalScroll)
    ) {
        ScrollAxis::Horizontal
    } else {
        ScrollAxis::Vertical
    }
}

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
        KeyRepeat, choose_integer_scale, key_code, pointer_scroll_event, refresh_hz_from_millihz,
        scroll_event,
    };
    use crate::{
        event::{KeyCode, Modifiers, ScrollPhase, ScrollUnit},
        math::Point,
    };

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

    #[test]
    fn a_wl_output_mode_refresh_becomes_a_frame_budget_hint() {
        assert_eq!(refresh_hz_from_millihz(60_000), Some(60.0));
        assert_eq!(refresh_hz_from_millihz(120_000), Some(120.0));
        assert_eq!(refresh_hz_from_millihz(143_981), Some(143.981));
        assert_eq!(refresh_hz_from_millihz(0), None, "0 = compositor 未给 mode");
        assert_eq!(refresh_hz_from_millihz(-60_000), None);
    }

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
        assert_eq!(key_code(4242), KeyCode::Unidentified(4242));
    }

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

    #[test]
    fn repeated_key_down_still_emits_text_on_the_xkb_fallback() {
        use crate::event::{KeyState, should_emit_text};

        assert!(should_emit_text(KeyState::Down, false, false, false));
        assert!(
            should_emit_text(KeyState::Down, true, false, false),
            "合成重复必须继续产生文本（P0 回归）"
        );
        assert!(!should_emit_text(KeyState::Down, true, true, false));
    }

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

    #[test]
    fn pointer_scroll_events_are_translated_to_the_pure_scroll_enum() {
        use wayland_client::{WEnum, protocol::wl_pointer};

        use crate::platform::wayland::scroll::{ScrollAxis, ScrollEvent, ScrollSource};

        assert_eq!(
            pointer_scroll_event(&wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(wl_pointer::AxisSource::Finger),
            }),
            Some(ScrollEvent::Source(ScrollSource::Finger)),
            "finger 源必须映射到 ScrollSource::Finger（RBL-02 的手势源）"
        );
        assert_eq!(
            pointer_scroll_event(&wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(wl_pointer::AxisSource::WheelTilt),
            }),
            Some(ScrollEvent::Source(ScrollSource::Wheel)),
            "wheel_tilt 是横向倾斜的滚轮 ⇒ 按滚轮"
        );
        assert_eq!(
            pointer_scroll_event(&wl_pointer::Event::AxisSource {
                axis_source: WEnum::Unknown(42),
            }),
            Some(ScrollEvent::Source(ScrollSource::Unknown)),
            "未知枚举值必须降级成 Unknown（按滚轮），不得 panic"
        );
        assert_eq!(
            pointer_scroll_event(&wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                discrete: 1,
            }),
            Some(ScrollEvent::Discrete {
                axis: ScrollAxis::Vertical,
                notches: 1,
            }),
            "axis_discrete 的 discrete 是格数，必须原样进 Discrete（v5–v7 唯一通路）"
        );
        assert_eq!(
            pointer_scroll_event(&wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value120: 120,
            }),
            Some(ScrollEvent::Value120 {
                axis: ScrollAxis::Vertical,
                value120: 120,
            })
        );
        assert_eq!(
            pointer_scroll_event(&wl_pointer::Event::Axis {
                time: 0,
                axis: WEnum::Value(wl_pointer::Axis::HorizontalScroll),
                value: 15.0,
            }),
            Some(ScrollEvent::Continuous {
                axis: ScrollAxis::Horizontal,
                value: 15.0,
            }),
            "水平轴必须保留（逐轴归一的前提）"
        );
        assert_eq!(
            pointer_scroll_event(&wl_pointer::Event::AxisStop {
                time: 0,
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
            }),
            Some(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            }),
            "axis_stop 必须带上轴号（NBL-01：逐轴 stop，不能当整手势结束）"
        );
        assert_eq!(
            pointer_scroll_event(&wl_pointer::Event::Frame),
            Some(ScrollEvent::Frame)
        );
    }

    #[test]
    fn non_scroll_pointer_events_are_not_translated() {
        use wayland_client::protocol::wl_pointer;

        assert_eq!(
            pointer_scroll_event(&wl_pointer::Event::Motion {
                time: 0,
                surface_x: 0.0,
                surface_y: 0.0,
            }),
            None
        );
    }
}

use std::{rc::Rc, sync::Arc};

use windows::Win32::Foundation::HWND;

use crate::{
    error::EngineError,
    math::{DevicePixelRatio, DeviceRect, LogicalRect},
    platform::{
        dispatcher::Dispatcher,
        event::{MessageContext, PlatformEvent, StampedEvent, TranslateState},
        windows::{
            a11y::WindowsA11y,
            dpi,
            ime::{ImeApplied, ImeCommand},
            placement,
        },
    },
    window::WindowId,
};

mod native_window;
mod pump;
mod recovery;
mod resize;
mod wndproc;

pub(crate) use pump::run;
pub(crate) use resize::{ModalTick, ResizeFrameDriver, ResizeStep};
pub(crate) use wndproc::wndproc;

#[cfg(test)]
mod tests;

pub(crate) struct WndProcState {
    dispatcher: Arc<Dispatcher>,
    events: Vec<StampedEvent>,
    translate: TranslateState,
    wheel_lines: f32,
    last_client_pos: (i32, i32),
    min_track_size: (u32, u32),
    max_track_size: Option<(u32, u32)>,
    limits: placement::ClientLimits,
    scale: DevicePixelRatio,
    decorations: crate::window::Decorations,
    resize_border: (i32, i32),
    chrome_dirty: bool,
    close_requested: bool,
    display_recovery: bool,
    mouse_tracking: bool,
    a11y: WindowsA11y,
    suppress_capture_cancel: bool,
    cursor: crate::paint::style::Cursor,
    drag_regions: Vec<LogicalRect>,
    size_move: bool,
    modal_entries: u32,
    suppress_move: bool,
    pending_error: Option<EngineError>,
    #[cfg(test)]
    last_dark_titlebar: Option<bool>,
    driver: Option<Rc<ResizeFrameDriver>>,
}

impl WndProcState {
    pub(crate) fn new(
        dispatcher: Arc<Dispatcher>,
        hwnd: HWND,
        window: WindowId,
        limits: placement::ClientLimits,
        decorations: crate::window::Decorations,
        driver: Option<Rc<ResizeFrameDriver>>,
        a11y_consumer_epoch: crate::a11y::adapter::A11yConsumerEpoch,
    ) -> Result<Self, EngineError> {
        let dpi = dpi::window_dpi(hwnd);
        let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
            factor: dpi as f32 / dpi::BASE_DPI as f32,
        })?;
        let (min_track_size, max_track_size) = limits.track_sizes(dpi, decorations);
        Ok(Self {
            dispatcher: dispatcher.clone(),
            events: Vec::with_capacity(8),
            translate: TranslateState::default(),
            wheel_lines: pump::wheel_lines_from_system(),
            last_client_pos: (0, 0),
            min_track_size,
            max_track_size,
            limits,
            scale,
            decorations,
            resize_border: dpi::resize_border(dpi),
            chrome_dirty: true,
            close_requested: false,
            display_recovery: false,
            mouse_tracking: false,
            a11y: WindowsA11y::new(hwnd, dispatcher, window, a11y_consumer_epoch)?,
            suppress_capture_cancel: false,
            cursor: crate::paint::style::Cursor::Default,
            drag_regions: Vec::new(),
            size_move: false,
            modal_entries: 0,
            suppress_move: false,
            pending_error: None,
            #[cfg(test)]
            last_dark_titlebar: None,
            driver,
        })
    }

    pub(crate) fn a11y_mut(&mut self) -> &mut WindowsA11y {
        &mut self.a11y
    }

    fn wake(&self) {
        self.dispatcher.wake();
    }

    pub(crate) fn dispatcher(&self) -> &Arc<Dispatcher> {
        &self.dispatcher
    }

    pub(crate) fn scale(&self) -> DevicePixelRatio {
        self.scale
    }

    pub(crate) fn ime_composing(&self) -> bool {
        self.translate.ime().is_composing()
    }

    pub(crate) fn note_dpi(&mut self, dpi: u32, scale: DevicePixelRatio) {
        self.scale = scale;
        let (min_track_size, max_track_size) = self.limits.track_sizes(dpi, self.decorations);
        self.min_track_size = min_track_size;
        self.max_track_size = max_track_size;
        self.resize_border = dpi::resize_border(dpi);
    }

    pub(crate) fn decorations(&self) -> crate::window::Decorations {
        self.decorations
    }

    pub(crate) fn resize_border(&self) -> (i32, i32) {
        self.resize_border
    }

    pub(crate) fn note_chrome_dirty(&mut self) {
        self.chrome_dirty = true;
    }

    pub(crate) fn take_chrome_dirty(&mut self) -> bool {
        std::mem::take(&mut self.chrome_dirty)
    }

    pub(crate) fn client_limits(&self) -> placement::ClientLimits {
        self.limits
    }

    pub(crate) fn context(&self, ticks_ms: u32) -> MessageContext {
        MessageContext {
            ticks_ms,
            wheel_lines: self.wheel_lines,
            scale: self.scale,
        }
    }

    pub(crate) fn refresh_wheel_lines(&mut self) {
        self.wheel_lines = pump::wheel_lines_from_system();
    }

    pub(crate) fn note_client_pos(&mut self, x: i32, y: i32) {
        self.last_client_pos = (x, y);
    }

    pub(crate) fn client_pos(&self) -> (i32, i32) {
        self.last_client_pos
    }

    pub(crate) fn push_stamped(&mut self, event: PlatformEvent, ticks_ms: u32) {
        self.push_stamped_event(StampedEvent {
            event,
            ticks_ms,
            scale: self.scale,
        });
    }

    pub(crate) fn push_now(&mut self, event: PlatformEvent) {
        let ticks_ms = pump::tick_now();
        self.push_stamped(event, ticks_ms);
    }

    pub(crate) fn push_stamped_event(&mut self, stamped: StampedEvent) {
        let is_move = matches!(
            &stamped.event,
            PlatformEvent::Pointer {
                kind: crate::platform::event::PointerKind::Move,
                ..
            }
        );
        if is_move
            && let Some(last) = self.events.last_mut()
            && matches!(
                last.event,
                PlatformEvent::Pointer {
                    kind: crate::platform::event::PointerKind::Move,
                    ..
                }
            )
        {
            *last = stamped;
            return;
        }
        self.events.push(stamped);
    }

    pub(crate) fn take_events(&mut self) -> Vec<StampedEvent> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn take_close_request(&mut self) -> bool {
        std::mem::take(&mut self.close_requested)
    }

    pub(crate) fn note_display_recovery(&mut self) {
        self.display_recovery = true;
    }

    pub(crate) fn take_display_recovery(&mut self) -> bool {
        std::mem::take(&mut self.display_recovery)
    }

    pub(crate) fn ime_enabled(&self) -> bool {
        self.translate.ime().enabled()
    }

    pub(crate) fn plan_ime_enabled(&mut self, enabled: bool) -> Option<ImeCommand> {
        self.translate.ime_mut().plan_enabled(enabled)
    }

    pub(crate) fn plan_ime_cursor_area(&mut self, area: Option<DeviceRect>) -> Option<ImeCommand> {
        self.translate.ime_mut().plan_cursor_area(area)
    }

    pub(crate) fn plan_ime_release(&mut self) -> Option<ImeCommand> {
        self.translate.ime_mut().plan_release_before_destroy()
    }

    pub(crate) fn plan_ime_reassert(&mut self) -> Option<ImeCommand> {
        self.translate.ime_mut().plan_reassert_disabled()
    }

    pub(crate) fn note_ime_applied(&mut self, applied: ImeApplied) {
        self.translate.ime_mut().note_applied(applied);
    }

    pub(crate) fn note_ime_reasserted(&mut self, applied: ImeApplied) {
        self.translate.ime_mut().note_reasserted(applied);
    }

    pub(crate) fn modifiers(&self) -> crate::event::Modifiers {
        self.translate.modifiers()
    }

    pub(crate) fn reset_transient_input(&mut self) {
        self.translate.reset_transient();
    }

    pub(crate) fn end_message_batch(&mut self) {
        self.translate.end_message_batch();
    }

    pub(crate) fn set_cursor(&mut self, cursor: crate::paint::style::Cursor) {
        self.cursor = cursor;
    }

    pub(crate) fn drag_regions(&self) -> &[LogicalRect] {
        &self.drag_regions
    }

    pub(crate) fn set_drag_regions(&mut self, regions: &[LogicalRect]) {
        if self.drag_regions == regions {
            return;
        }
        self.drag_regions.clear();
        self.drag_regions.extend_from_slice(regions);
    }

    pub(crate) fn begin_size_move(&mut self) {
        self.size_move = true;
        self.modal_entries = self.modal_entries.saturating_add(1);
        self.dispatcher.enter_modal_loop();
        if self.dispatcher.take_frame_wake() {
            self.dispatcher.wake();
        }
    }

    pub(crate) fn end_size_move(&mut self) {
        self.size_move = false;
        if self.modal_entries > 0 {
            self.modal_entries -= 1;
            self.dispatcher.exit_modal_loop();
        }
    }

    pub(crate) fn resize_step(&self, client: (u32, u32)) -> Option<ResizeStep> {
        if !self.size_move {
            return None;
        }
        let driver = self.driver.as_ref()?;
        Some(ResizeStep::new(Rc::clone(driver), self.scale, client))
    }

    pub(crate) fn modal_tick_step(&self) -> Option<ModalTick> {
        let driver = self.driver.as_ref()?;
        Some(ModalTick::new(
            Rc::clone(driver),
            Arc::clone(&self.dispatcher),
        ))
    }

    pub(crate) fn note_pending_error(&mut self, error: EngineError) {
        self.pending_error.get_or_insert(error);
    }

    pub(crate) fn take_pending_error(&mut self) -> Option<EngineError> {
        self.pending_error.take()
    }
}

impl Drop for WndProcState {
    fn drop(&mut self) {
        for _ in 0..self.modal_entries {
            self.dispatcher.exit_modal_loop();
        }
    }
}

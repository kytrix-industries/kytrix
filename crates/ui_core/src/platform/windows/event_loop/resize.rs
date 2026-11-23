use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
};

use crate::{
    app::App,
    error::EngineError,
    math::{Scale, Size},
    platform::{
        dispatcher::Dispatcher,
        windows::composition::{self, Composition},
    },
    window::{Window, WindowId},
};

pub(crate) struct ResizeStep {
    driver: Rc<ResizeFrameDriver>,
    scale: Scale,
    client: (u32, u32),
}

impl ResizeStep {
    pub(crate) fn new(driver: Rc<ResizeFrameDriver>, scale: Scale, client: (u32, u32)) -> Self {
        Self {
            driver,
            scale,
            client,
        }
    }

    pub(crate) fn run(self) -> Option<EngineError> {
        self.driver.frame(self.scale, self.client)
    }
}

pub(crate) struct ModalTick {
    driver: Rc<ResizeFrameDriver>,
    dispatcher: Arc<Dispatcher>,
}

impl ModalTick {
    pub(crate) fn new(driver: Rc<ResizeFrameDriver>, dispatcher: Arc<Dispatcher>) -> Self {
        Self { driver, dispatcher }
    }

    pub(crate) fn run(self) -> Option<EngineError> {
        self.driver.modal_tick(&self.dispatcher)
    }
}

pub(crate) struct ResizeFrameDriver {
    app: Weak<RefCell<App>>,
    window: WindowId,
    composition: Arc<Mutex<Composition>>,
}

impl ResizeFrameDriver {
    pub(crate) fn new(
        app: Weak<RefCell<App>>,
        window: WindowId,
        composition: Arc<Mutex<Composition>>,
    ) -> Self {
        Self {
            app,
            window,
            composition,
        }
    }

    pub(crate) fn frame(&self, scale: Scale, client: (u32, u32)) -> Option<EngineError> {
        let app = self.app.upgrade()?;
        let mut app = app.try_borrow_mut().ok()?;
        if let Err(error) = app.poll_renderers() {
            return Some(error);
        }
        if client.0 == 0 || client.1 == 0 {
            return None;
        }
        let window = app.window_mut(self.window)?;
        if window.scale() != scale {
            window.set_scale(scale);
        }
        window.set_size(Size::new(
            scale.to_logical(client.0 as f32),
            scale.to_logical(client.1 as f32),
        ));
        window.set_surface(client);
        let presented = app
            .window(self.window)
            .map(Window::presented_surface)
            .unwrap_or(client);
        {
            let mut target = composition::lock(&self.composition);
            if let Err(error) = target.set_geometry(presented, client) {
                return Some(error);
            }
        }
        match app.frame_forced(self.window) {
            Some(Ok(_)) | None => None,
            Some(Err(error)) => Some(error),
        }
    }

    pub(crate) fn modal_tick(&self, dispatcher: &Arc<Dispatcher>) -> Option<EngineError> {
        let app = self.app.upgrade()?;
        let mut app = app.try_borrow_mut().ok()?;
        if !dispatcher.take_frame_wake() {
            return None;
        }
        if let Err(error) = app.poll_renderers() {
            return Some(error);
        }
        for task in dispatcher.drain_tasks() {
            task.run(&mut app);
        }
        app.run_frame_hook();
        let windows: Vec<WindowId> = app.windows().map(|(id, _)| id).collect();
        for window in windows {
            match app.frame(window) {
                None | Some(Ok(_)) => {}
                Some(Err(error)) => return Some(error),
            }
        }
        if app.has_deferred_frames() {
            dispatcher.wake_later(DEFERRED_WAKE_RETRY);
        }
        None
    }
}

const DEFERRED_WAKE_RETRY: std::time::Duration = std::time::Duration::from_millis(4);

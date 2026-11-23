use std::{os::fd::AsFd, path::PathBuf};

use wayland_client::{
    QueueHandle,
    protocol::{wl_buffer, wl_compositor, wl_pointer, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};

use crate::platform::{
    linux::surface::WaylandState,
    wayland::cursor::{
        self, CursorApplyState, choose_image, cursor_candidates, icon_roots, parse_xcursor,
        theme_chain, theme_size,
    },
};

const MAX_CACHED_IMAGES: usize = 8;

const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

struct CursorImage {
    cursor: crate::paint::style::Cursor,
    size: u32,
    _file: std::fs::File,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    hotspot: (i32, i32),
    pixels: (i32, i32),
}

pub(crate) struct CursorManager {
    shape_manager: Option<WpCursorShapeManagerV1>,
    shape_device: Option<WpCursorShapeDeviceV1>,
    shm: Option<wl_shm::WlShm>,
    surface: Option<wl_surface::WlSurface>,
    images: Vec<CursorImage>,
    apply: CursorApplyState,
}

impl CursorManager {
    pub(crate) fn new(
        shape_manager: Option<WpCursorShapeManagerV1>,
        shm: Option<wl_shm::WlShm>,
    ) -> Self {
        Self {
            shape_manager,
            shape_device: None,
            shm,
            surface: None,
            images: Vec::with_capacity(MAX_CACHED_IMAGES),
            apply: CursorApplyState::default(),
        }
    }

    pub(crate) fn pointer_created(
        &mut self,
        pointer: &wl_pointer::WlPointer,
        queue: &QueueHandle<WaylandState>,
    ) {
        if let Some(device) = self.shape_device.take() {
            device.destroy();
        }
        self.apply.pointer_recreated();
        self.shape_device = self
            .shape_manager
            .as_ref()
            .map(|manager| manager.get_pointer(pointer, queue, ()));
    }

    pub(crate) fn pointer_gone(&mut self) {
        if let Some(device) = self.shape_device.take() {
            device.destroy();
        }
        self.apply.pointer_recreated();
    }

    pub(crate) fn hide(&mut self, pointer: Option<&wl_pointer::WlPointer>, serial: u32) {
        if let Some(pointer) = pointer
            && serial != 0
        {
            pointer.set_cursor(serial, None, 0, 0);
        }
        self.apply.hidden();
    }

    pub(crate) fn apply(
        &mut self,
        pointer: Option<&wl_pointer::WlPointer>,
        serial: u32,
        cursor: crate::paint::style::Cursor,
        scale: i32,
        compositor: &wl_compositor::WlCompositor,
        queue: &QueueHandle<WaylandState>,
    ) -> bool {
        let (Some(pointer), true) = (pointer, serial != 0) else {
            return false;
        };
        let size = theme_size(env_cursor_size(), scale);
        if !self.apply.needs_apply(cursor, size) {
            return true;
        }
        let applied = if let Some(device) = self.shape_device.as_ref() {
            let Ok(shape) = Shape::try_from(cursor::spec(cursor).shape) else {
                return false;
            };
            device.set_shape(serial, shape);
            true
        } else {
            self.apply_from_theme(pointer, serial, cursor, size, compositor, queue)
        };
        if applied {
            self.apply.note_applied(cursor, size);
        }
        applied
    }

    fn apply_from_theme(
        &mut self,
        pointer: &wl_pointer::WlPointer,
        serial: u32,
        cursor: crate::paint::style::Cursor,
        size: u32,
        compositor: &wl_compositor::WlCompositor,
        queue: &QueueHandle<WaylandState>,
    ) -> bool {
        let Some(shm) = self.shm.clone() else {
            return false;
        };
        if self.surface.is_none() {
            self.surface = Some(compositor.create_surface(queue, ()));
        }
        let Some(surface) = self.surface.clone() else {
            return false;
        };
        let index = match self
            .images
            .iter()
            .position(|entry| entry.cursor == cursor && entry.size == size)
        {
            Some(index) => index,
            None => {
                let Some(image) = load_theme_image(cursor, size) else {
                    return false;
                };
                let Some(entry) = upload_image(&shm, cursor, size, &image, queue) else {
                    return false;
                };
                self.images.push(entry);
                self.images.len() - 1
            }
        };
        let (buffer, hotspot, pixels) = {
            let entry = &self.images[index];
            (entry.buffer.clone(), entry.hotspot, entry.pixels)
        };
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, pixels.0, pixels.1);
        surface.commit();
        pointer.set_cursor(serial, Some(&surface), hotspot.0, hotspot.1);
        while self.images.len() > MAX_CACHED_IMAGES {
            let stale = self.images.remove(0);
            stale.buffer.destroy();
            stale.pool.destroy();
        }
        true
    }
}

impl Drop for CursorManager {
    fn drop(&mut self) {
        if let Some(device) = self.shape_device.take() {
            device.destroy();
        }
        for entry in self.images.drain(..) {
            entry.buffer.destroy();
            entry.pool.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
    }
}

fn env_cursor_size() -> Option<u32> {
    std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
}

fn load_theme_image(cursor: crate::paint::style::Cursor, size: u32) -> Option<XcursorImageBuf> {
    let spec = cursor::spec(cursor);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_dirs: Vec<PathBuf> = std::env::var("XDG_DATA_DIRS").map_or_else(
        |_| {
            vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ]
        },
        |dirs| {
            dirs.split(':')
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .collect()
        },
    );
    let roots = icon_roots(home, &data_dirs);
    let theme = std::env::var("XCURSOR_THEME")
        .ok()
        .filter(|theme| !theme.is_empty())
        .unwrap_or_else(|| "default".to_owned());
    let lookup_roots = roots.clone();
    let chain = theme_chain(&theme, |name| {
        lookup_roots
            .iter()
            .find_map(|root| std::fs::read_to_string(root.join(name).join("index.theme")).ok())
    });
    for path in cursor_candidates(&roots, &chain, spec.theme_names) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(images) = parse_xcursor(&bytes) else {
            continue;
        };
        let Some(index) = choose_image(&images, size) else {
            continue;
        };
        let image = images[index];
        return Some(XcursorImageBuf {
            width: image.width,
            height: image.height,
            xhot: image.xhot,
            yhot: image.yhot,
            pixels: image.pixels.to_vec(),
        });
    }
    None
}

struct XcursorImageBuf {
    width: u32,
    height: u32,
    xhot: u32,
    yhot: u32,
    pixels: Vec<u8>,
}

fn upload_image(
    shm: &wl_shm::WlShm,
    cursor: crate::paint::style::Cursor,
    size: u32,
    image: &XcursorImageBuf,
    queue: &QueueHandle<WaylandState>,
) -> Option<CursorImage> {
    let stride = image.width.checked_mul(4)?;
    let byte_len = (stride as usize).checked_mul(image.height as usize)?;
    if byte_len == 0 || byte_len > MAX_IMAGE_BYTES {
        return None;
    }
    let file = create_shm_file(byte_len)?;
    {
        use std::io::Write;
        let mut writer = &file;
        writer.write_all(&image.pixels).ok()?;
    }
    let pool = shm.create_pool(file.as_fd(), byte_len as i32, queue, ());
    let buffer = pool.create_buffer(
        0,
        image.width as i32,
        image.height as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        queue,
        (),
    );
    Some(CursorImage {
        cursor,
        size,
        _file: file,
        pool,
        buffer,
        hotspot: (image.xhot as i32, image.yhot as i32),
        pixels: (image.width as i32, image.height as i32),
    })
}

fn create_shm_file(len: usize) -> Option<std::fs::File> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("kytrix-cursor-{}-{serial}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .ok()?;
    let _ = std::fs::remove_file(&path);
    file.set_len(len as u64).ok()?;
    Some(file)
}

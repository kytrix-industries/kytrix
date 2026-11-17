//! 光标下发（R10-3）：`wp_cursor_shape_v1` 首选，`wl_cursor` 主题 + `wl_shm` 回退。
//!
//! 协议事实（`wayland.xml` 的 `wl_pointer.set_cursor`）：*"Set the pointer surface, i.e., the
//! surface that contains the pointer image (cursor). … The cursor actually changes only if the
//! pointer focus for this device is one of the requesting client's surfaces."* —— 客户端不发就
//! 永远是 compositor 的默认箭头，而且 `serial` 必须是**最近一次 `wl_pointer.enter`/按键**的
//! serial（新焦点下第一个合法凭证）。
//!
//! 两条路径：
//!
//! 1. `wp_cursor_shape_manager_v1`（cursor-shape-v1，GNOME/KWin/wlroots 全线支持）：只发
//!    `set_shape(serial, shape)`，没有 buffer 管理，compositor 自己按缩放选图 ⇒ 永远不会糊。
//! 2. 回退：读 Xcursor 主题文件（`~/.icons` / `$XDG_DATA_DIRS/icons`），选标称尺寸最接近的
//!    图像，拷进 `wl_shm` 池，`attach` 到光标 surface 后 `set_cursor(serial, surface, hotspot)`。
//!    winit / Qt / GTK4 / SDL3 都是这条双路实现。
//!
//! 纯逻辑（形状映射表、Xcursor 解析、主题继承链、尺寸选择）在 `platform::wayland::cursor`，
//! 这里只做协议对象与文件/`wl_shm` 的搬运。

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
        self, choose_image, cursor_candidates, icon_roots, parse_xcursor, theme_chain, theme_size,
    },
};

/// 回退路径里缓存的最大条目数（每个 `(cursor, size)` 一份 shm 池；8 个形状 × 64 KiB 上限）。
const MAX_CACHED_IMAGES: usize = 8;

/// 单张光标图像的字节上限（128×128×4 = 64 KiB；超过说明主题文件不可信）。
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

/// 一个已上传的光标图像（回退路径）。
struct CursorImage {
    /// 引擎光标（缓存键的一半）。
    cursor: crate::paint::style::Cursor,
    /// 目标像素尺寸（缓存键的另一半）。
    size: u32,
    /// shm 池的底层文件（buffer 的内存来源；shm 池不复制数据，必须活到 buffer 被释放）。
    _file: std::fs::File,
    /// shm 池（`wl_buffer` 引用它的内存）。
    pool: wl_shm_pool::WlShmPool,
    /// 已上传的 buffer。
    buffer: wl_buffer::WlBuffer,
    /// 热点（buffer 像素坐标）。
    hotspot: (i32, i32),
    /// 图像像素尺寸。
    pixels: (i32, i32),
}

/// 光标下发状态（`WaylandState` 持有）。
pub(crate) struct CursorManager {
    /// `wp_cursor_shape_manager_v1`（可选 global；缺失 ⇒ 走主题回退）。
    shape_manager: Option<WpCursorShapeManagerV1>,
    /// 当前 pointer 的 shape device（device 绑定在 pointer 对象上，pointer 重建时必须重建）。
    shape_device: Option<WpCursorShapeDeviceV1>,
    /// `wl_shm`（回退路径上传图像用；缺失 ⇒ 只能走 cursor-shape-v1）。
    shm: Option<wl_shm::WlShm>,
    /// 回退路径的光标 surface（懒创建，整个进程复用）。
    surface: Option<wl_surface::WlSurface>,
    /// 已上传图像缓存。
    images: Vec<CursorImage>,
    /// 最近一次下发的形状（同值不重复下发；`None` = 未下发 / 已隐藏）。
    applied: Option<crate::paint::style::Cursor>,
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
            applied: None,
        }
    }

    /// pointer 被创建 / 重建：重建它的 shape device。
    pub(crate) fn pointer_created(
        &mut self,
        pointer: &wl_pointer::WlPointer,
        queue: &QueueHandle<WaylandState>,
    ) {
        if let Some(device) = self.shape_device.take() {
            device.destroy();
        }
        self.applied = None;
        self.shape_device = self
            .shape_manager
            .as_ref()
            .map(|manager| manager.get_pointer(pointer, queue, ()));
    }

    /// pointer 被销毁（seat 能力撤销）：丢掉 device 与已下发状态。
    pub(crate) fn pointer_gone(&mut self) {
        if let Some(device) = self.shape_device.take() {
            device.destroy();
        }
        self.applied = None;
    }

    /// `wl_pointer.leave`：隐藏光标，避免指针移到别的 surface 上时残留。
    ///
    /// 之后 `applied` 归零：再次 enter 时即使形状没变也要重新下发（否则会一直隐藏）。
    pub(crate) fn hide(&mut self, pointer: Option<&wl_pointer::WlPointer>, serial: u32) {
        if let Some(pointer) = pointer
            && serial != 0
        {
            pointer.set_cursor(serial, None, 0, 0);
        }
        self.applied = None;
    }

    /// 下发光标形状：cursor-shape-v1 优先，否则读主题 + shm 上传。
    ///
    /// 返回 `false` 表示两条路径都不可用（没有 cursor-shape、也没有可用的主题/`wl_shm`）——
    /// 调用方不必重试（每次 enter / 形状变化都会再试）。
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
        if self.applied == Some(cursor) {
            return true;
        }
        if let Some(device) = self.shape_device.as_ref() {
            let Ok(shape) = Shape::try_from(cursor::spec(cursor).shape) else {
                return false;
            };
            device.set_shape(serial, shape);
            self.applied = Some(cursor);
            return true;
        }
        self.apply_from_theme(pointer, serial, cursor, scale, compositor, queue)
    }

    /// 回退路径：主题文件 → shm buffer → `attach` + `commit` + `set_cursor`。
    fn apply_from_theme(
        &mut self,
        pointer: &wl_pointer::WlPointer,
        serial: u32,
        cursor: crate::paint::style::Cursor,
        scale: i32,
        compositor: &wl_compositor::WlCompositor,
        queue: &QueueHandle<WaylandState>,
    ) -> bool {
        let Some(shm) = self.shm.clone() else {
            return false;
        };
        let size = theme_size(env_cursor_size(), scale);
        // 缓存命中：surface 上已经 attach 过这张 buffer，只要重新 set_cursor。
        let cached = self
            .images
            .iter()
            .find(|entry| entry.cursor == cursor && entry.size == size)
            .map(|entry| (entry.hotspot, entry.pixels));
        if let Some((hotspot, _)) = cached {
            let Some(surface) = self.surface.as_ref() else {
                return false;
            };
            pointer.set_cursor(serial, Some(surface), hotspot.0, hotspot.1);
            self.applied = Some(cursor);
            return true;
        }
        let Some(image) = load_theme_image(cursor, size) else {
            return false;
        };
        let Some(entry) = upload_image(&shm, cursor, size, &image, queue) else {
            return false;
        };
        if self.surface.is_none() {
            self.surface = Some(compositor.create_surface(queue, ()));
        }
        let Some(surface) = self.surface.as_ref() else {
            return false;
        };
        let hotspot = entry.hotspot;
        let pixels = entry.pixels;
        surface.attach(Some(&entry.buffer), 0, 0);
        surface.damage(0, 0, pixels.0, pixels.1);
        surface.commit();
        pointer.set_cursor(serial, Some(surface), hotspot.0, hotspot.1);
        self.images.push(entry);
        while self.images.len() > MAX_CACHED_IMAGES {
            let stale = self.images.remove(0);
            stale.buffer.destroy();
            stale.pool.destroy();
        }
        self.applied = Some(cursor);
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

/// `XCURSOR_SIZE`（逻辑尺寸提示；缺失 ⇒ 用 `cursor::DEFAULT_CURSOR_SIZE`）。
fn env_cursor_size() -> Option<u32> {
    std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
}

/// 从主题里读出一张符合 `cursor` / `size` 的图像（读不到 ⇒ `None`，光标保持 compositor 默认）。
///
/// 顺序：`XCURSOR_THEME`（默认 `default`）→ `index.theme` 的 `Inherits` 链 → 兜底 `default`
/// → 每个主题的候选别名（`left_ptr` / `hand2` / `xterm` …）→ 每个搜索根。
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
    // `index.theme` 的查找也用同一套搜索根（`<root>/<theme>/index.theme`）。
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

/// 已读入内存的一张光标图像（`XcursorImage` 借用文件字节，这里拷贝出来以便上传后丢掉文件）。
struct XcursorImageBuf {
    width: u32,
    height: u32,
    xhot: u32,
    yhot: u32,
    pixels: Vec<u8>,
}

/// 把一张图像拷进新的 `wl_shm` 池并创建 buffer。
///
/// 池的底层用 `XDG_RUNTIME_DIR`（回退 `/tmp`）里的临时文件：`wl_shm` 只要求 fd 指向一块共享、
/// 可被 compositor `mmap` 的内存，`shm_open` 的对象本质上就是 tmpfs 文件 —— 直接建文件再
/// `unlink` 等价，而且不需要引入 `libc`/`nix` 依赖（本仓没有它们）。
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
        pool: pool,
        buffer,
        hotspot: (image.xhot as i32, image.yhot as i32),
        pixels: (image.width as i32, image.height as i32),
    })
}

/// 建一个 `wl_shm` 池用的临时文件（`XDG_RUNTIME_DIR` 优先，回退 `/tmp`），已 `set_len`。
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
    // 立即 unlink：fd 仍然有效，目录里不留垃圾（进程异常退出也不会残留）。
    let _ = std::fs::remove_file(&path);
    file.set_len(len as u64).ok()?;
    Some(file)
}

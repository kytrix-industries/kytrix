//! 光标：`wp_cursor_shape_v1` 的形状码映射（首选路径）与 Xcursor 主题解析（回退路径），R10-3。
//!
//! Wayland 上**光标图像唯一的来源是客户端**：`wl_pointer.set_cursor(serial, surface, hotspot)`
//! 或 `wp_cursor_shape_device_v1.set_shape(serial, shape)`。不调用 ⇒ compositor 用它的默认箭头，
//! 并且在整个窗口生命周期内不变 —— 文本域没有 I-beam、边框 resize 没有形状反馈（R10-3）。
//!
//! 两条路径的 `serial` 都必须是**最近一次 `wl_pointer.enter` 的 serial**（协议：
//! *"The cursor actually changes only if the pointer focus … is one of the requesting client's
//! surfaces"*，而 enter 的 serial 是新焦点下第一个合法凭证）。
//!
//! 本模块只做纯逻辑：形状映射表、Xcursor 文件解析、主题目录/继承链解析、尺寸选择。协议对象
//! 与 `wl_shm` 上传在 `platform/linux/cursor.rs`。

use std::path::PathBuf;

use smol_str::SmolStr;

use crate::paint::style::Cursor;

/// 一个引擎光标形状的两条下发通道所需的全部信息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorSpec {
    /// `wp_cursor_shape_device_v1::Shape` 的协议值（[cursor-shape-v1] 的枚举）。
    ///
    /// 值直接写死而不用 `wayland_protocols` 的枚举：本模块必须能在没有 wayland 依赖的
    /// 平台上单测（见模块文档），协议侧只是 `Shape::try_from(code)`。
    ///
    /// [cursor-shape-v1]: https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/cursor-shape/cursor-shape-v1.xml
    pub shape: u32,
    /// 回退路径（`wl_cursor` 主题）的候选文件名，**顺序即偏好**（不同主题的别名不同：
    /// `left_ptr` 是 X11 传统名、`default` 是 CSS 名、`hand2` 是 Xcursor 常见名）。
    pub theme_names: &'static [&'static str],
}

/// 引擎 [`Cursor`] → 协议形状码 + 主题候选名（表驱动，R10-3）。
///
/// 形状码与 CSS `cursor` 关键字的对应关系取自 cursor-shape-v1 的枚举名（`default` /
/// `pointer` / `text` / `not_allowed` / `grab` / `grabbing` / `ew_resize` / `ns_resize`），
/// 与 Chromium `WaylandCursor::SetCursor`、Qt `QWaylandCursor` 的映射一致。
#[must_use]
pub(crate) fn spec(cursor: Cursor) -> CursorSpec {
    match cursor {
        Cursor::Default => CursorSpec {
            shape: SHAPE_DEFAULT,
            theme_names: &["left_ptr", "default", "arrow"],
        },
        Cursor::Pointer => CursorSpec {
            shape: SHAPE_POINTER,
            theme_names: &["pointer", "hand2", "hand"],
        },
        Cursor::Text => CursorSpec {
            shape: SHAPE_TEXT,
            theme_names: &["text", "xterm", "ibeam"],
        },
        Cursor::NotAllowed => CursorSpec {
            shape: SHAPE_NOT_ALLOWED,
            theme_names: &["not-allowed", "crossed_circle", "forbidden"],
        },
        Cursor::Grab => CursorSpec {
            shape: SHAPE_GRAB,
            theme_names: &["openhand", "grab", "hand1"],
        },
        Cursor::Grabbing => CursorSpec {
            shape: SHAPE_GRABBING,
            theme_names: &["closedhand", "grabbing", "hand2"],
        },
        Cursor::ResizeHorizontal => CursorSpec {
            shape: SHAPE_EW_RESIZE,
            theme_names: &["sb_h_double_arrow", "ew-resize", "size_hor"],
        },
        Cursor::ResizeVertical => CursorSpec {
            shape: SHAPE_NS_RESIZE,
            theme_names: &["sb_v_double_arrow", "ns-resize", "size_ver"],
        },
    }
}

// cursor-shape-v1 的 `Shape` 枚举值（只列本引擎用到的；其余按 CSS 语义无关）。
const SHAPE_DEFAULT: u32 = 1;
const SHAPE_POINTER: u32 = 4;
const SHAPE_TEXT: u32 = 9;
const SHAPE_NOT_ALLOWED: u32 = 15;
const SHAPE_GRAB: u32 = 16;
const SHAPE_GRABBING: u32 = 17;
const SHAPE_EW_RESIZE: u32 = 26;
const SHAPE_NS_RESIZE: u32 = 27;

/// 主题里没有 `XCURSOR_SIZE` 时的逻辑尺寸（X11 传统默认 24 px，与 GTK/Qt 一致）。
pub(crate) const DEFAULT_CURSOR_SIZE: u32 = 24;

/// 主题尺寸的合理区间：太小无法辨识，太大在 4K 上会吃掉半屏。
pub(crate) const MIN_CURSOR_SIZE: u32 = 8;
pub(crate) const MAX_CURSOR_SIZE: u32 = 128;

/// 回退路径要用的主题图像尺寸（逻辑尺寸 × 整数 scale，夹紧到合理区间）。
///
/// 回退路径没有 `wp_cursor_shape_v1` 那样的「compositor 帮你按缩放选图」：图像按目标像素
/// 尺寸选档，buffer scale 恒 1（热点也按原始像素给），因此这里的尺寸必须是**物理像素**。
#[must_use]
pub(crate) fn theme_size(hint: Option<u32>, scale: i32) -> u32 {
    let base = hint.unwrap_or(DEFAULT_CURSOR_SIZE);
    let scaled = base.saturating_mul(scale.max(1) as u32);
    scaled.clamp(MIN_CURSOR_SIZE, MAX_CURSOR_SIZE)
}

// ── Xcursor 文件解析（回退路径）─────────────────────────────────────────────

/// Xcursor 文件魔数（小端读出来的 `Xcur`）。
const XCURSOR_MAGIC: u32 = 0x7275_6358;
const XCURSOR_MAGIC_SWAPPED: u32 = 0x5863_7572;
/// 支持的文件版本（1.0）。
const XCURSOR_FILE_VERSION: u32 = 0x0001_0000;
/// 图像 chunk 的类型码（`XCURSOR_IMAGE_TYPE`）。
const XCURSOR_IMAGE_TYPE: u32 = 0xfffd_0002;
/// 文件头固定 16 字节；每个 TOC 项 12 字节；图像 chunk 头 36 字节。
const FILE_HEADER_LEN: usize = 16;
const TOC_ENTRY_LEN: usize = 12;
const IMAGE_HEADER_LEN: usize = 36;

/// Xcursor 文件里的一个图像。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct XcursorImage<'a> {
    /// 该图像的标称尺寸（Xcursor 用 `subtype` 表示，24 / 32 / 48 …）。
    pub nominal_size: u32,
    /// 实际像素宽。
    pub width: u32,
    /// 实际像素高。
    pub height: u32,
    /// 热点 x（必须落在图像内）。
    pub xhot: u32,
    /// 热点 y。
    pub yhot: u32,
    /// `width × height` 个小端 ARGB32（**预乘 alpha**）—— 与 `wl_shm::Format::Argb8888`
    /// 的内存布局完全一致，可以直接 memcpy 进 shm 池。
    pub pixels: &'a [u8],
}

/// Xcursor 解析失败的原因（只在回退路径里用于「换下一个候选文件」，不冒泡给用户）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum XcursorError {
    /// magic 不是 `Xcur`（不是 Xcursor 文件，或根本不是光标文件）。
    BadMagic,
    /// 大端字节序的 Xcursor 文件（Linux/macOS 桌面全是小端；遇到就跳过这个候选）。
    UnsupportedEndianness,
    /// 版本不是 1.0。
    UnsupportedVersion,
    /// 文件被截断 / chunk 位置越界。
    Truncated,
    /// 没有任何可用的图像 chunk。
    NoImages,
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let slice = bytes.get(offset..end)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// 解析一个 Xcursor 文件，返回其中**全部**图像（按文件顺序）。
///
/// 只接受小端文件（Linux/macOS 桌面全部如此）；损坏的 TOC 项被跳过而不是整体失败，只有
/// 「一个图像都没解析出来」才是 `NoImages`（与 libXcursor 的容错行为一致）。
pub(crate) fn parse_xcursor(bytes: &[u8]) -> Result<Vec<XcursorImage<'_>>, XcursorError> {
    let magic = read_u32(bytes, 0).ok_or(XcursorError::Truncated)?;
    if magic == XCURSOR_MAGIC_SWAPPED {
        return Err(XcursorError::UnsupportedEndianness);
    }
    if magic != XCURSOR_MAGIC {
        return Err(XcursorError::BadMagic);
    }
    let version = read_u32(bytes, 8).ok_or(XcursorError::Truncated)?;
    if version != XCURSOR_FILE_VERSION {
        return Err(XcursorError::UnsupportedVersion);
    }
    let ntoc = read_u32(bytes, 12).ok_or(XcursorError::Truncated)?;
    let toc_start = FILE_HEADER_LEN;
    // 每个 TOC 项 12 字节；`ntoc` 来自文件，先做溢出/越界检查再分配。
    let toc_len = (ntoc as usize)
        .checked_mul(TOC_ENTRY_LEN)
        .ok_or(XcursorError::Truncated)?;
    if toc_start
        .checked_add(toc_len)
        .filter(|end| *end <= bytes.len())
        .is_none()
    {
        return Err(XcursorError::Truncated);
    }
    let mut images = Vec::with_capacity(ntoc.min(16) as usize);
    for index in 0..ntoc as usize {
        let entry = toc_start + index * TOC_ENTRY_LEN;
        let (Some(chunk_type), Some(subtype), Some(position)) = (
            read_u32(bytes, entry),
            read_u32(bytes, entry + 4),
            read_u32(bytes, entry + 8),
        ) else {
            continue;
        };
        if chunk_type != XCURSOR_IMAGE_TYPE {
            continue;
        }
        let at = position as usize;
        let Some(header) = read_u32(bytes, at) else {
            continue;
        };
        if header as usize != IMAGE_HEADER_LEN {
            continue;
        }
        let (Some(width), Some(height), Some(xhot), Some(yhot)) = (
            read_u32(bytes, at + 16),
            read_u32(bytes, at + 20),
            read_u32(bytes, at + 24),
            read_u32(bytes, at + 28),
        ) else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        let pixels_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|count| count.checked_mul(4))
            .ok_or(XcursorError::Truncated)?;
        let pixels_start = at + IMAGE_HEADER_LEN;
        let Some(pixels) = bytes.get(pixels_start..pixels_start + pixels_len) else {
            continue;
        };
        images.push(XcursorImage {
            nominal_size: subtype,
            width,
            height,
            // 热点越界在真实主题里存在（生成工具不校验），夹紧到图像内避免 compositor 收到
            // 非法热点（协议不校验，但越界热点会让指针定位不可预测）。
            xhot: xhot.min(width - 1),
            yhot: yhot.min(height - 1),
            pixels,
        });
    }
    if images.is_empty() {
        return Err(XcursorError::NoImages);
    }
    Ok(images)
}

/// 选标称尺寸最接近 `size` 的图像下标（平局取**较大**的那张：宁可略大也不模糊）。
pub(crate) fn choose_image(images: &[XcursorImage<'_>], size: u32) -> Option<usize> {
    // (与目标尺寸的距离, 标称尺寸, 下标)
    let mut best: Option<(u32, u32, usize)> = None;
    for (index, image) in images.iter().enumerate() {
        let delta = image.nominal_size.abs_diff(size);
        let better = match best {
            None => true,
            Some((best_delta, best_nominal, _)) => {
                delta < best_delta || (delta == best_delta && image.nominal_size > best_nominal)
            }
        };
        if better {
            best = Some((delta, image.nominal_size, index));
        }
    }
    best.map(|(_, _, index)| index)
}

// ── 主题目录 / 继承链（回退路径）────────────────────────────────────────────

/// 光标主题的搜索根，顺序与 libXcursor 的 `XcursorGetTheme`/`XcursorLibraryLoadImages` 一致：
/// 用户目录优先，其次是 `$XDG_DATA_DIRS/icons`（默认 `/usr/local/share` + `/usr/share`），
/// 最后是历史遗留的 `/usr/share/pixmaps`（老主题只放这里）。
#[must_use]
pub(crate) fn icon_roots(home: Option<PathBuf>, data_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(data_dirs.len() + 2);
    if let Some(home) = home {
        roots.push(home.join(".icons"));
    }
    for dir in data_dirs {
        roots.push(dir.join("icons"));
        roots.push(dir.join("pixmaps"));
    }
    roots
}

/// `index.theme` 里的 `Inherits=`（主题继承链），逗号分隔、允许空白。
#[must_use]
pub(crate) fn inherits(index_theme: &str) -> Option<SmolStr> {
    for line in index_theme.lines() {
        let line = line.trim();
        // 只认 `[Icon Theme]` 段里的键：无关段里的同名键（理论上不存在，但主题文件是外部输入）。
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("Inherits") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(SmolStr::new(value));
            }
        }
    }
    None
}

/// 主题继承链：`theme` → 它声明的 `Inherits`（可多级）→ 兜底 `default`。
///
/// `lookup` 负责给一个主题名返回它的 `index.theme` 内容（`None` = 主题不存在）。
/// 深度上限 8 且去重：主题文件是外部输入，`A Inherits B` / `B Inherits A` 的环必须收敛。
#[must_use]
pub(crate) fn theme_chain(
    theme: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Vec<SmolStr> {
    const MAX_DEPTH: usize = 8;
    let mut chain: Vec<SmolStr> = Vec::with_capacity(3);
    let mut pending: Vec<SmolStr> = vec![SmolStr::new(theme)];
    while let Some(name) = pending.pop() {
        if name.is_empty() || chain.contains(&name) {
            continue;
        }
        if chain.len() >= MAX_DEPTH {
            break;
        }
        let index_theme = lookup(name.as_str());
        chain.push(name);
        if let Some(inherited) = index_theme.as_deref().and_then(inherits) {
            // 继承列表按声明顺序入栈的反序压栈（pop 后按声明顺序处理）。
            for next in inherited.split(',').rev() {
                pending.push(SmolStr::new(next.trim()));
            }
        }
    }
    if !chain.iter().any(|seen| seen.as_str() == "default") {
        chain.push(SmolStr::new_static("default"));
    }
    chain
}

/// 候选光标文件路径：**主题优先于别名，别名优先于搜索根**（与 libXcursor 的查找顺序一致）。
///
/// 只生成路径，不碰文件系统 —— 调用方按顺序 `std::fs::read` 第一个存在的即可。
#[must_use]
pub(crate) fn cursor_candidates(
    roots: &[PathBuf],
    chain: &[SmolStr],
    names: &[&str],
) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(chain.len() * names.len() * roots.len());
    for theme in chain {
        for name in names {
            for root in roots {
                candidates.push(root.join(theme.as_str()).join("cursors").join(name));
            }
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        CursorSpec, DEFAULT_CURSOR_SIZE, MAX_CURSOR_SIZE, MIN_CURSOR_SIZE, XcursorError,
        XcursorImage, choose_image, cursor_candidates, icon_roots, inherits, parse_xcursor, spec,
        theme_chain, theme_size,
    };
    use crate::paint::style::Cursor;

    /// 全部 8 个 [`Cursor`] 变体（新增变体时这里少一项 ⇒ 下面的覆盖测试会失败）。
    const ALL: [Cursor; 8] = [
        Cursor::Default,
        Cursor::Pointer,
        Cursor::Text,
        Cursor::NotAllowed,
        Cursor::Grab,
        Cursor::Grabbing,
        Cursor::ResizeHorizontal,
        Cursor::ResizeVertical,
    ];

    /// R10-3 的形状映射表：每个引擎光标必须映射到**唯一**的 cursor-shape-v1 形状码，
    /// 且回退主题名非空（否则回退路径永远找不到图像）。
    #[test]
    fn every_cursor_maps_to_a_distinct_protocol_shape() {
        let mut shapes = Vec::new();
        for cursor in ALL {
            let CursorSpec { shape, theme_names } = spec(cursor);
            assert!(
                shape >= 1,
                "{cursor:?} 必须映射到合法的 Shape（0 不是枚举值）"
            );
            assert!(!theme_names.is_empty(), "{cursor:?} 必须有回退主题名");
            assert!(
                theme_names.iter().all(|name| !name.is_empty()),
                "{cursor:?} 的主题别名不得为空串"
            );
            assert!(
                !shapes.contains(&shape),
                "{cursor:?} 的 Shape {shape} 与其它光标重复"
            );
            shapes.push(shape);
        }
        assert_eq!(shapes.len(), ALL.len());
    }

    /// 具体的 CSS 对应关系（照 cursor-shape-v1 的枚举实现，回退时改错会让用户看到错的箭头）。
    #[test]
    fn cursor_shapes_match_the_protocol_enum() {
        assert_eq!(spec(Cursor::Default).shape, 1, "default");
        assert_eq!(spec(Cursor::Pointer).shape, 4, "pointer");
        assert_eq!(spec(Cursor::Text).shape, 9, "text");
        assert_eq!(spec(Cursor::NotAllowed).shape, 15, "not_allowed");
        assert_eq!(spec(Cursor::Grab).shape, 16, "grab");
        assert_eq!(spec(Cursor::Grabbing).shape, 17, "grabbing");
        assert_eq!(spec(Cursor::ResizeHorizontal).shape, 26, "ew_resize");
        assert_eq!(spec(Cursor::ResizeVertical).shape, 27, "ns_resize");
    }

    /// 主题别名必须覆盖 X11 传统名（`left_ptr` / `hand2` / `xterm`）：绝大多数发行版主题只装
    /// 传统名，只写 CSS 名会回退失败。
    #[test]
    fn theme_aliases_cover_the_x11_names() {
        assert!(spec(Cursor::Default).theme_names.contains(&"left_ptr"));
        assert!(spec(Cursor::Pointer).theme_names.contains(&"hand2"));
        assert!(spec(Cursor::Text).theme_names.contains(&"xterm"));
        assert!(
            spec(Cursor::ResizeHorizontal)
                .theme_names
                .contains(&"sb_h_double_arrow")
        );
    }

    #[test]
    fn theme_size_scales_with_the_output_and_clamps() {
        assert_eq!(theme_size(None, 1), DEFAULT_CURSOR_SIZE);
        assert_eq!(theme_size(None, 2), DEFAULT_CURSOR_SIZE * 2);
        assert_eq!(theme_size(Some(48), 1), 48);
        assert_eq!(
            theme_size(None, 0),
            DEFAULT_CURSOR_SIZE,
            "非法 scale 按 1 处理"
        );
        assert_eq!(
            theme_size(Some(1), 1),
            MIN_CURSOR_SIZE,
            "过小的尺寸被抬到下限"
        );
        assert_eq!(
            theme_size(Some(1024), 1),
            MAX_CURSOR_SIZE,
            "过大的尺寸被压到上限（否则 4K 上光标吃掉半屏）"
        );
    }

    /// 构造一个最小可用的 Xcursor 文件（两个图像：24 与 48）。
    fn sample_xcursor() -> (Vec<u8>, Vec<(u32, u32, u32, u32)>) {
        // 返回 (文件字节, 每个图像的 (nominal, width, height, offset))
        let images = [(24u32, 2u32, 2u32), (48, 1, 1)];
        let header_len = 16 + images.len() * 12;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x7275_6358u32.to_le_bytes()); // magic "Xcur"
        bytes.extend_from_slice(&16u32.to_le_bytes()); // header size
        bytes.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // version 1.0
        bytes.extend_from_slice(&(images.len() as u32).to_le_bytes());
        let mut positions = Vec::new();
        let mut offset = header_len;
        for (nominal, width, height) in images {
            positions.push(offset as u32);
            bytes.extend_from_slice(&0xfffd_0002u32.to_le_bytes()); // image type
            bytes.extend_from_slice(&nominal.to_le_bytes()); // subtype = nominal size
            bytes.extend_from_slice(&(offset as u32).to_le_bytes()); // position
            offset += 36 + (width * height * 4) as usize;
        }
        for (nominal, width, height) in images {
            bytes.extend_from_slice(&36u32.to_le_bytes()); // chunk header size
            bytes.extend_from_slice(&0xfffd_0002u32.to_le_bytes()); // type
            bytes.extend_from_slice(&nominal.to_le_bytes()); // subtype
            bytes.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // image version
            bytes.extend_from_slice(&width.to_le_bytes());
            bytes.extend_from_slice(&height.to_le_bytes());
            bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // xhot（越界，测试夹紧）
            bytes.extend_from_slice(&0u32.to_le_bytes()); // yhot
            bytes.extend_from_slice(&1u32.to_le_bytes()); // delay
            let pixels = vec![0xABu8; (width * height * 4) as usize];
            bytes.extend_from_slice(&pixels);
        }
        let expected = vec![(24u32, 2u32, 2u32, positions[0]), (48, 1, 1, positions[1])];
        (bytes, expected)
    }

    #[test]
    fn xcursor_images_are_parsed_with_hotspot_and_pixels() {
        let (bytes, expected) = sample_xcursor();
        let images = parse_xcursor(&bytes).expect("示例文件必须能解析");
        assert_eq!(images.len(), 2);
        for (image, (nominal, width, height, position)) in images.iter().zip(&expected) {
            assert_eq!(image.nominal_size, *nominal);
            assert_eq!(image.width, *width);
            assert_eq!(image.height, *height);
            assert_eq!(
                image.pixels.len(),
                (*width * *height * 4) as usize,
                "像素区必须正好是 w×h×4 字节"
            );
            assert_eq!(image.xhot, width - 1, "越界热点被夹紧到图像内");
            assert_eq!(image.yhot, 0);
            // 像素必须落在 chunk 之后（位置由 TOC 给出），而不是把头部当成像素。
            assert_eq!(
                u32::from_le_bytes([
                    bytes[*position as usize],
                    bytes[*position as usize + 1],
                    bytes[*position as usize + 2],
                    bytes[*position as usize + 3],
                ]),
                36,
                "chunk 头必须是 36 字节"
            );
        }
        assert_eq!(images[0].pixels, [0xABu8; 16]);
    }

    #[test]
    fn xcursor_errors_are_classified_so_the_fallback_can_try_the_next_file() {
        assert_eq!(parse_xcursor(&[]).unwrap_err(), XcursorError::Truncated);
        let mut bad_magic = sample_xcursor().0;
        bad_magic[0] = b'Y';
        assert_eq!(
            parse_xcursor(&bad_magic).unwrap_err(),
            XcursorError::BadMagic
        );
        let mut swapped = Vec::new();
        swapped.extend_from_slice(&0x5863_7572u32.to_le_bytes());
        swapped.extend_from_slice(&[0; 12]);
        assert_eq!(
            parse_xcursor(&swapped).unwrap_err(),
            XcursorError::UnsupportedEndianness,
            "大端文件必须被识别（而不是当成损坏）"
        );

        let mut bad_version = sample_xcursor().0;
        bad_version[8] = 9;
        assert_eq!(
            parse_xcursor(&bad_version).unwrap_err(),
            XcursorError::UnsupportedVersion
        );

        // 只有非图像 chunk：没有可用图像。
        let mut no_images = Vec::new();
        no_images.extend_from_slice(&0x7275_6358u32.to_le_bytes());
        no_images.extend_from_slice(&16u32.to_le_bytes());
        no_images.extend_from_slice(&0x0001_0000u32.to_le_bytes());
        no_images.extend_from_slice(&1u32.to_le_bytes());
        no_images.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        no_images.extend_from_slice(&0u32.to_le_bytes());
        no_images.extend_from_slice(&16u32.to_le_bytes());
        assert_eq!(
            parse_xcursor(&no_images).unwrap_err(),
            XcursorError::NoImages
        );
    }

    #[test]
    fn truncated_toc_is_reported_instead_of_panicking() {
        let (mut bytes, _) = sample_xcursor();
        // TOC 声称有 100 项，但文件只有 2 项的空间。
        bytes[12..16].copy_from_slice(&100u32.to_le_bytes());
        assert_eq!(parse_xcursor(&bytes).unwrap_err(), XcursorError::Truncated);
    }

    #[test]
    fn the_closest_nominal_size_wins_and_ties_take_the_larger_image() {
        let (bytes, _) = sample_xcursor();
        let images = parse_xcursor(&bytes).expect("解析");
        assert_eq!(choose_image(&images, 24), Some(0));
        assert_eq!(choose_image(&images, 48), Some(1));
        assert_eq!(
            choose_image(&images, 32),
            Some(0),
            "32 离 24 更近（差 8 对 16）"
        );
        assert_eq!(
            choose_image(&images, 36),
            Some(1),
            "等距（各差 12）时取较大的一张，避免糊"
        );
        assert_eq!(choose_image(&images, 8), Some(0));
        assert_eq!(
            choose_image(&images, 96),
            Some(1),
            "超出全部档位时取最近的大图"
        );
        assert_eq!(choose_image(&[], 24), None);
    }

    /// 一个手写的图像数组（`choose_image` 的边界：等距 / 单图）。
    #[test]
    fn choose_image_handles_a_single_image() {
        let pixels = [0u8; 4];
        let only = XcursorImage {
            nominal_size: 32,
            width: 1,
            height: 1,
            xhot: 0,
            yhot: 0,
            pixels: &pixels,
        };
        assert_eq!(choose_image(std::slice::from_ref(&only), 999), Some(0));
    }

    #[test]
    fn icon_roots_follow_the_libxcursor_search_order() {
        let roots = icon_roots(
            Some(PathBuf::from("/home/u")),
            &[
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ],
        );
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/home/u/.icons"),
                PathBuf::from("/usr/local/share/icons"),
                PathBuf::from("/usr/local/share/pixmaps"),
                PathBuf::from("/usr/share/icons"),
                PathBuf::from("/usr/share/pixmaps"),
            ]
        );
        // 没有 HOME（容器里的服务进程）时只剩系统目录。
        let roots = icon_roots(None, &[PathBuf::from("/usr/share")]);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/usr/share/icons"),
                PathBuf::from("/usr/share/pixmaps")
            ]
        );
    }

    #[test]
    fn inherits_is_read_from_the_index_theme() {
        assert_eq!(inherits(""), None);
        assert_eq!(inherits("[Icon Theme]\nName=Foo\n"), None);
        assert_eq!(
            inherits("[Icon Theme]\nInherits=Adwaita,hicolor\n"),
            Some(smoll("Adwaita,hicolor"))
        );
        assert_eq!(
            inherits("[Icon Theme]\nInherits = Adwaita \n"),
            Some(smoll("Adwaita")),
            "等号两侧空白要容忍（真实主题文件写法不一）"
        );
    }

    fn smoll(text: &str) -> smol_str::SmolStr {
        smol_str::SmolStr::new(text)
    }

    #[test]
    fn theme_chain_follows_inherits_and_always_ends_with_default() {
        let chain = theme_chain("Adwaita", |name| {
            (name == "Adwaita").then(|| "[Icon Theme]\nInherits=hicolor\n".to_owned())
        });
        assert_eq!(
            chain,
            vec![smoll("Adwaita"), smoll("hicolor"), smoll("default")]
        );

        // 无 index.theme 的主题：直接兜底 default。
        let chain = theme_chain("Custom", |_| None);
        assert_eq!(chain, vec![smoll("Custom"), smoll("default")]);

        // 主题本身就是 default：不重复。
        let chain = theme_chain("default", |_| None);
        assert_eq!(chain, vec![smoll("default")]);
    }

    /// 主题继承成环（外部输入）必须收敛，不能无限展开。
    #[test]
    fn theme_chain_terminates_on_cycles() {
        let chain = theme_chain("A", |name| {
            let next = match name {
                "A" => "B",
                "B" => "A",
                _ => return None,
            };
            Some(format!("[Icon Theme]\nInherits={next}\n"))
        });
        assert_eq!(chain, vec![smoll("A"), smoll("B"), smoll("default")]);
    }

    #[test]
    fn cursor_candidates_prefer_the_theme_then_the_alias_then_the_root() {
        let roots = vec![
            PathBuf::from("/usr/share/icons"),
            PathBuf::from("/usr/share/pixmaps"),
        ];
        let chain = vec![smoll("Adwaita"), smoll("default")];
        let candidates = cursor_candidates(&roots, &chain, &["text", "xterm"]);
        let expected: Vec<PathBuf> = vec![
            "/usr/share/icons/Adwaita/cursors/text".into(),
            "/usr/share/pixmaps/Adwaita/cursors/text".into(),
            "/usr/share/icons/Adwaita/cursors/xterm".into(),
            "/usr/share/pixmaps/Adwaita/cursors/xterm".into(),
            "/usr/share/icons/default/cursors/text".into(),
            "/usr/share/pixmaps/default/cursors/text".into(),
            "/usr/share/icons/default/cursors/xterm".into(),
            "/usr/share/pixmaps/default/cursors/xterm".into(),
        ];
        assert_eq!(candidates, expected);
    }
}

use std::path::PathBuf;

use smol_str::SmolStr;

use crate::paint::style::Cursor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorSpec {
    pub shape: u32,
    pub theme_names: &'static [&'static str],
}

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

const SHAPE_DEFAULT: u32 = 1;
const SHAPE_POINTER: u32 = 4;
const SHAPE_TEXT: u32 = 9;
const SHAPE_NOT_ALLOWED: u32 = 15;
const SHAPE_GRAB: u32 = 16;
const SHAPE_GRABBING: u32 = 17;
const SHAPE_EW_RESIZE: u32 = 26;
const SHAPE_NS_RESIZE: u32 = 27;

pub(crate) const DEFAULT_CURSOR_SIZE: u32 = 24;

pub(crate) const MIN_CURSOR_SIZE: u32 = 8;
pub(crate) const MAX_CURSOR_SIZE: u32 = 128;

#[must_use]
pub(crate) fn theme_size(hint: Option<u32>, scale: i32) -> u32 {
    let base = hint.unwrap_or(DEFAULT_CURSOR_SIZE);
    let scaled = base.saturating_mul(scale.max(1) as u32);
    scaled.clamp(MIN_CURSOR_SIZE, MAX_CURSOR_SIZE)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorApplyKey {
    pub generation: u32,
    pub cursor: Cursor,
    pub size: u32,
}

#[derive(Debug, Default)]
pub(crate) struct CursorApplyState {
    generation: u32,
    applied: Option<CursorApplyKey>,
}

impl CursorApplyState {
    pub(crate) fn pointer_recreated(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.applied = None;
    }

    pub(crate) fn hidden(&mut self) {
        self.applied = None;
    }

    #[must_use]
    pub(crate) fn needs_apply(&self, cursor: Cursor, size: u32) -> bool {
        self.applied
            != Some(CursorApplyKey {
                generation: self.generation,
                cursor,
                size,
            })
    }

    pub(crate) fn note_applied(&mut self, cursor: Cursor, size: u32) {
        self.applied = Some(CursorApplyKey {
            generation: self.generation,
            cursor,
            size,
        });
    }
}

const XCURSOR_MAGIC: u32 = 0x7275_6358;
const XCURSOR_MAGIC_SWAPPED: u32 = 0x5863_7572;
const XCURSOR_FILE_VERSION: u32 = 0x0001_0000;
const XCURSOR_IMAGE_TYPE: u32 = 0xfffd_0002;
const FILE_HEADER_LEN: usize = 16;
const TOC_ENTRY_LEN: usize = 12;
const IMAGE_HEADER_LEN: usize = 36;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct XcursorImage<'a> {
    pub nominal_size: u32,
    pub width: u32,
    pub height: u32,
    pub xhot: u32,
    pub yhot: u32,
    pub pixels: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum XcursorError {
    BadMagic,
    UnsupportedEndianness,
    UnsupportedVersion,
    Truncated,
    NoImages,
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let slice = bytes.get(offset..end)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

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

pub(crate) fn choose_image(images: &[XcursorImage<'_>], size: u32) -> Option<usize> {
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

#[must_use]
pub(crate) fn inherits(index_theme: &str) -> Option<SmolStr> {
    for line in index_theme.lines() {
        let line = line.trim();
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
        CursorApplyState, CursorSpec, DEFAULT_CURSOR_SIZE, MAX_CURSOR_SIZE, MIN_CURSOR_SIZE,
        XcursorError, XcursorImage, choose_image, cursor_candidates, icon_roots, inherits,
        parse_xcursor, spec, theme_chain, theme_size,
    };
    use crate::paint::style::Cursor;

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

    fn sample_xcursor() -> (Vec<u8>, Vec<(u32, u32, u32, u32)>) {
        let images = [(24u32, 2u32, 2u32), (48, 1, 1)];
        let header_len = 16 + images.len() * 12;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x7275_6358u32.to_le_bytes());
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&0x0001_0000u32.to_le_bytes());
        bytes.extend_from_slice(&(images.len() as u32).to_le_bytes());
        let mut positions = Vec::new();
        let mut offset = header_len;
        for (nominal, width, height) in images {
            positions.push(offset as u32);
            bytes.extend_from_slice(&0xfffd_0002u32.to_le_bytes());
            bytes.extend_from_slice(&nominal.to_le_bytes());
            bytes.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += 36 + (width * height * 4) as usize;
        }
        for (nominal, width, height) in images {
            bytes.extend_from_slice(&36u32.to_le_bytes());
            bytes.extend_from_slice(&0xfffd_0002u32.to_le_bytes());
            bytes.extend_from_slice(&nominal.to_le_bytes());
            bytes.extend_from_slice(&0x0001_0000u32.to_le_bytes());
            bytes.extend_from_slice(&width.to_le_bytes());
            bytes.extend_from_slice(&height.to_le_bytes());
            bytes.extend_from_slice(&u32::MAX.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&1u32.to_le_bytes());
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

        let chain = theme_chain("Custom", |_| None);
        assert_eq!(chain, vec![smoll("Custom"), smoll("default")]);

        let chain = theme_chain("default", |_| None);
        assert_eq!(chain, vec![smoll("default")]);
    }

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

    #[test]
    fn the_same_shape_and_size_is_not_resent() {
        let mut state = CursorApplyState::default();
        assert!(state.needs_apply(Cursor::Text, 24));
        state.note_applied(Cursor::Text, 24);
        assert!(
            !state.needs_apply(Cursor::Text, 24),
            "没变就不该再发 set_shape / set_cursor"
        );
        assert!(state.needs_apply(Cursor::Pointer, 24), "形状变了必须重发");
        state.note_applied(Cursor::Pointer, 24);
        assert!(
            !state.needs_apply(Cursor::Pointer, 24),
            "切回同一个形状（未重新下发）仍视为已下发"
        );
        assert!(
            state.needs_apply(Cursor::Text, 24),
            "Text 是更早下发的那个形状，当前 surface 上是 Pointer ⇒ 必须重发"
        );
    }

    #[test]
    fn every_shape_switch_reapplies_even_when_all_shapes_are_cached() {
        let mut state = CursorApplyState::default();
        for cursor in [Cursor::Default, Cursor::Pointer, Cursor::Text] {
            assert!(state.needs_apply(cursor, 24), "{cursor:?} 首次下发");
            state.note_applied(cursor, 24);
        }
        assert!(
            state.needs_apply(Cursor::Default, 24),
            "缓存命中不等于已下发：surface 上现在是 Text 的 buffer"
        );
        state.note_applied(Cursor::Default, 24);
        assert!(state.needs_apply(Cursor::Text, 24));
        state.note_applied(Cursor::Text, 24);
        assert!(state.needs_apply(Cursor::Pointer, 24));
    }

    #[test]
    fn a_recreated_pointer_device_forces_a_reapply() {
        let mut state = CursorApplyState::default();
        state.note_applied(Cursor::Grab, 32);
        assert!(!state.needs_apply(Cursor::Grab, 32));

        state.pointer_recreated();
        assert!(
            state.needs_apply(Cursor::Grab, 32),
            "pointer 重建后旧 device 已销毁 ⇒ 必须重新 set_shape / set_cursor"
        );
        state.note_applied(Cursor::Grab, 32);

        state.pointer_recreated();
        state.pointer_recreated();
        assert!(
            state.needs_apply(Cursor::Grab, 32),
            "代数只增不减，连续重建仍然要求重发"
        );
    }

    #[test]
    fn a_hidden_cursor_is_reapplied_on_the_next_enter() {
        let mut state = CursorApplyState::default();
        state.note_applied(Cursor::Text, 24);
        state.hidden();
        assert!(
            state.needs_apply(Cursor::Text, 24),
            "离开时我们把光标设成 None，回来必须重新给图像"
        );
    }

    #[test]
    fn a_scale_change_reapplies_the_theme_image() {
        let mut state = CursorApplyState::default();
        state.note_applied(Cursor::Default, theme_size(None, 1));
        assert!(!state.needs_apply(Cursor::Default, theme_size(None, 1)));
        assert!(
            state.needs_apply(Cursor::Default, theme_size(None, 2)),
            "2× 输出上必须换成 2 倍像素的图（否则光标糊）"
        );
    }

    #[test]
    fn an_unsent_shape_is_not_recorded_as_applied() {
        let state = CursorApplyState::default();
        assert!(state.needs_apply(Cursor::NotAllowed, 24));
        assert!(state.needs_apply(Cursor::NotAllowed, 24));
    }
}

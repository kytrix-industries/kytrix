use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use smol_str::{SmolStr, format_smolstr};

use crate::{
    math::{DevicePixelRatio, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit},
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{Border, Gradient, GradientKind, Image},
        style::{Clip, PaintSpec, ShadowSpec, Shadows, Style},
        style_builder::StyleBuilder,
    },
    test_support::app::TestApp,
    window::WindowOptions,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotCase {
    pub scene: SnapshotScene,
    pub theme: SnapshotTheme,
    pub size: LogicalSize,
    pub scale: DevicePixelRatio,
}

impl Default for SnapshotCase {
    fn default() -> Self {
        Self {
            scene: SnapshotScene::Shapes,
            theme: SnapshotTheme::Light,
            size: LogicalSize::new(px(800.0), px(600.0)),
            scale: DevicePixelRatio::ONE,
        }
    }
}

impl SnapshotCase {
    #[must_use]
    pub fn golden_name(&self) -> SmolStr {
        format_smolstr!(
            "{}_{}_{}",
            self.scene.token(),
            scale_token(self.scale.factor()),
            self.theme.token()
        )
    }

    #[must_use]
    pub fn golden_path(&self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(device_dir())
            .join(format!("{}.png", self.golden_name()))
    }
}

#[must_use]
pub fn device_dir() -> &'static str {
    #[cfg(windows)]
    {
        "warp"
    }
    #[cfg(target_os = "macos")]
    {
        "metal-real"
    }
    #[cfg(target_os = "linux")]
    {
        "lavapipe"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnapshotScene {
    Shapes,
    Shadow,
    Gradient,
    Colors,
    Image,
    Clip,
    ClipSubpixel,
}

impl SnapshotScene {
    pub const ALL: [Self; 7] = [
        Self::Shapes,
        Self::Shadow,
        Self::Gradient,
        Self::Colors,
        Self::Image,
        Self::Clip,
        Self::ClipSubpixel,
    ];

    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Shapes => "shapes",
            Self::Shadow => "shadow",
            Self::Gradient => "gradient",
            Self::Colors => "colors",
            Self::Image => "image",
            Self::Clip => "clip",
            Self::ClipSubpixel => "clip_subpixel",
        }
    }

    #[must_use]
    pub fn size(self) -> LogicalSize {
        match self {
            Self::Shapes | Self::Shadow | Self::Gradient | Self::Clip | Self::ClipSubpixel => {
                LogicalSize::new(px(240.0), px(160.0))
            }
            Self::Colors | Self::Image => LogicalSize::new(px(160.0), px(120.0)),
        }
    }

    fn build(self, app: &mut TestApp, theme: SnapshotTheme) {
        let palette = theme.palette();
        let root = root_of(app);
        match self {
            Self::Shapes => build_shapes(app, root, &palette),
            Self::Shadow => build_shadow(app, root, &palette),
            Self::Gradient => build_gradient(app, root, &palette),
            Self::Colors => build_colors(app, root, &palette),
            Self::Image => build_image(app, root, &palette),
            Self::Clip => build_clip(app, root, &palette),
            Self::ClipSubpixel => build_clip_subpixel(app, root, &palette),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnapshotTheme {
    Light,
    Dark,
}

impl SnapshotTheme {
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn palette(self) -> Palette {
        match self {
            Self::Light => Palette {
                backdrop: Color::from_rgba8([0xF4, 0xF4, 0xF6, 0xFF]),
                surface: Color::from_rgba8([0xFF, 0xFF, 0xFF, 0xFF]),
                ink: Color::from_rgba8([0x1B, 0x1F, 0x24, 0xFF]),
                accent: Color::from_rgba8([0x00, 0x91, 0xFF, 0xFF]),
                muted: Color::from_rgba8([0x8B, 0x8D, 0x98, 0xFF]),
            },
            Self::Dark => Palette {
                backdrop: Color::from_rgba8([0x10, 0x14, 0x18, 0xFF]),
                surface: Color::from_rgba8([0x1B, 0x20, 0x27, 0xFF]),
                ink: Color::from_rgba8([0xE6, 0xE8, 0xEB, 0xFF]),
                accent: Color::from_rgba8([0x4C, 0x9A, 0xFF, 0xFF]),
                muted: Color::from_rgba8([0x6E, 0x74, 0x80, 0xFF]),
            },
        }
    }
}

struct Palette {
    backdrop: Color,
    surface: Color,
    ink: Color,
    accent: Color,
    muted: Color,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotImage {
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
}

impl SnapshotImage {
    pub fn save_png(&self, path: &Path) -> Result<(), GoldenError> {
        let image = image::RgbaImage::from_raw(self.width, self.height, self.rgba8.clone())
            .ok_or_else(|| GoldenError::GoldenDecode {
                path: path.to_path_buf(),
                detail: SmolStr::new_static("buffer size does not match dimensions"),
            })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| GoldenError::Io {
                path: parent.to_path_buf(),
                detail: SmolStr::new(error.to_string()),
            })?;
        }
        image.save(path).map_err(|error| GoldenError::Io {
            path: path.to_path_buf(),
            detail: SmolStr::new(error.to_string()),
        })
    }

    pub fn load_png(path: &Path) -> Result<Self, GoldenError> {
        let bytes = std::fs::read(path).map_err(|error| GoldenError::Io {
            path: path.to_path_buf(),
            detail: SmolStr::new(error.to_string()),
        })?;
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .map_err(|error| GoldenError::GoldenDecode {
                path: path.to_path_buf(),
                detail: SmolStr::new(error.to_string()),
            })?;
        let rgba = decoded.to_rgba8();
        Ok(Self {
            width: rgba.width(),
            height: rgba.height(),
            rgba8: rgba.into_raw(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    NoRenderer,
    Gpu { detail: SmolStr },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tolerance {
    pub per_channel: u8,
    pub max_mismatch_ppm: u32,
}

impl Tolerance {
    pub const EXACT: Self = Self {
        per_channel: 0,
        max_mismatch_ppm: 0,
    };

    pub const GEOMETRY: Self = Self {
        per_channel: 2,
        max_mismatch_ppm: 0,
    };

    #[must_use]
    pub fn allowed_mismatches(&self, pixels: u32) -> u32 {
        let allowed = u64::from(pixels) * u64::from(self.max_mismatch_ppm) / 1_000_000;
        u32::try_from(allowed).unwrap_or(u32::MAX)
    }
}

impl Default for Tolerance {
    fn default() -> Self {
        Self {
            per_channel: 2,
            max_mismatch_ppm: 1_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotDiff {
    pub max_channel_diff: u8,
    pub avg_channel_diff: f32,
    pub mismatched_pixels: u32,
    pub compared_pixels: u32,
    pub first_mismatch: Option<(u32, u32)>,
    pub mismatch_bounds: Option<PixelBounds>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelBounds {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

impl fmt::Display for PixelBounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "({},{})-({},{})",
            self.min_x, self.min_y, self.max_x, self.max_y
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum GoldenError {
    Snapshot(SnapshotError),
    Frame {
        detail: SmolStr,
    },
    Dimensions {
        actual: (u32, u32),
        golden: (u32, u32),
    },
    Io {
        path: PathBuf,
        detail: SmolStr,
    },
    GoldenDecode {
        path: PathBuf,
        detail: SmolStr,
    },
    Malformed {
        detail: SmolStr,
    },
    Diff {
        case: SmolStr,
        diff: SnapshotDiff,
        allowed: u32,
        golden: PathBuf,
        heatmap: PathBuf,
    },
}

impl fmt::Display for GoldenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(error) => write!(f, "snapshot failed: {error:?}"),
            Self::Frame { detail } => write!(f, "frame failed: {detail}"),
            Self::Dimensions { actual, golden } => write!(
                f,
                "snapshot is {}x{} but the golden is {}x{}",
                actual.0, actual.1, golden.0, golden.1
            ),
            Self::Io { path, detail } => {
                write!(f, "golden io failed at {}: {detail}", path.display())
            }
            Self::GoldenDecode { path, detail } => {
                write!(
                    f,
                    "golden at {} is not a valid png: {detail}",
                    path.display()
                )
            }
            Self::Malformed { detail } => write!(f, "snapshot image is malformed: {detail}"),
            Self::Diff {
                case,
                diff,
                allowed,
                golden,
                heatmap,
            } => {
                let percent = 100.0 * f64::from(diff.mismatched_pixels)
                    / f64::from(diff.compared_pixels.max(1));
                let first = diff.first_mismatch.map_or_else(
                    || SmolStr::new_static("-"),
                    |(x, y)| format_smolstr!("{x},{y}"),
                );
                let bounds = diff.mismatch_bounds.map_or_else(
                    || SmolStr::new_static("-"),
                    |bounds| format_smolstr!("{bounds}"),
                );
                write!(
                    f,
                    "golden `{case}` differs: {} mismatched of {} pixels ({percent:.3}%, allowed {allowed}), \
                     max channel diff {}, first mismatch ({first}), mismatch bounds {bounds}, \
                     golden {}, heatmap {}",
                    diff.mismatched_pixels,
                    diff.compared_pixels,
                    diff.max_channel_diff,
                    golden.display(),
                    heatmap.display()
                )
            }
        }
    }
}

pub fn compare(
    actual: &SnapshotImage,
    golden: &SnapshotImage,
    tolerance: Tolerance,
) -> Result<SnapshotDiff, GoldenError> {
    if (actual.width, actual.height) != (golden.width, golden.height) {
        return Err(GoldenError::Dimensions {
            actual: (actual.width, actual.height),
            golden: (golden.width, golden.height),
        });
    }
    let pixels = actual.width as usize * actual.height as usize;
    let expected = pixels * 4;
    if actual.rgba8.len() != expected || golden.rgba8.len() != expected {
        return Err(GoldenError::Malformed {
            detail: format_smolstr!(
                "{}x{} needs {expected} bytes but got {} (actual) / {} (golden)",
                actual.width,
                actual.height,
                actual.rgba8.len(),
                golden.rgba8.len()
            ),
        });
    }

    let mut max_channel_diff = 0u8;
    let mut sum = 0u64;
    let mut mismatched_pixels = 0u32;
    let mut first_mismatch = None;
    let mut mismatch_bounds: Option<PixelBounds> = None;
    for (index, (actual_pixel, golden_pixel)) in actual
        .rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .zip(golden.rgba8.as_chunks::<4>().0.iter())
        .enumerate()
    {
        let mut pixel_max = 0u8;
        for channel in 0..4 {
            let diff = actual_pixel[channel].abs_diff(golden_pixel[channel]);
            pixel_max = pixel_max.max(diff);
            sum += u64::from(diff);
        }
        max_channel_diff = max_channel_diff.max(pixel_max);
        if pixel_max > tolerance.per_channel {
            mismatched_pixels += 1;
            let x = (index % actual.width as usize) as u32;
            let y = (index / actual.width as usize) as u32;
            if first_mismatch.is_none() {
                first_mismatch = Some((x, y));
            }
            mismatch_bounds = Some(match mismatch_bounds {
                None => PixelBounds {
                    min_x: x,
                    min_y: y,
                    max_x: x,
                    max_y: y,
                },
                Some(bounds) => PixelBounds {
                    min_x: bounds.min_x.min(x),
                    min_y: bounds.min_y.min(y),
                    max_x: bounds.max_x.max(x),
                    max_y: bounds.max_y.max(y),
                },
            });
        }
    }
    let channels = (pixels * 4).max(1) as f32;
    Ok(SnapshotDiff {
        max_channel_diff,
        avg_channel_diff: sum as f32 / channels,
        mismatched_pixels,
        compared_pixels: pixels as u32,
        first_mismatch,
        mismatch_bounds,
    })
}

pub fn mount(app: &mut TestApp, scene: SnapshotScene, theme: SnapshotTheme) {
    scene.build(app, theme);
}

pub fn capture(case: SnapshotCase) -> Result<SnapshotImage, GoldenError> {
    let mut app = TestApp::with_options(WindowOptions {
        title: SmolStr::new_static("kytrix golden"),
        size: case.size,
        ..WindowOptions::default()
    });
    app.set_dpr(case.scale);
    mount(&mut app, case.scene, case.theme);
    app.drain().map_err(|error| GoldenError::Frame {
        detail: SmolStr::new(error.to_string()),
    })?;
    app.snapshot(case).map_err(GoldenError::Snapshot)
}

pub fn write_golden(case: SnapshotCase, image: &SnapshotImage) -> Result<(), GoldenError> {
    image.save_png(&case.golden_path())
}

pub fn assert_golden(
    case: SnapshotCase,
    tolerance: Tolerance,
) -> Result<SnapshotDiff, GoldenError> {
    let actual = capture(case)?;
    if bless_requested() {
        write_golden(case, &actual)?;
        eprintln!(
            "blessed by UI_CORE_BLESS: {} -> {}",
            case.golden_name(),
            case.golden_path().display()
        );
    }
    let path = case.golden_path();
    let golden = SnapshotImage::load_png(&path)?;
    let diff = compare(&actual, &golden, tolerance)?;
    let allowed = tolerance.allowed_mismatches(diff.compared_pixels);
    if diff.mismatched_pixels > allowed {
        let heatmap = write_heatmap(&case.golden_name(), &actual, &golden, tolerance)?;
        return Err(GoldenError::Diff {
            case: case.golden_name(),
            diff,
            allowed,
            golden: path,
            heatmap,
        });
    }
    Ok(diff)
}

pub fn gpu_guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn bless_requested() -> bool {
    std::env::var_os("UI_CORE_BLESS").is_some()
}

fn scale_token(scale: f32) -> SmolStr {
    let text = format_smolstr!("{scale:.2}");
    SmolStr::new(text.trim_end_matches('0').trim_end_matches('.'))
}

fn write_heatmap(
    name: &SmolStr,
    actual: &SnapshotImage,
    golden: &SnapshotImage,
    tolerance: Tolerance,
) -> Result<PathBuf, GoldenError> {
    let mut rgba = actual.rgba8.clone();
    for (pixel, reference) in rgba
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(golden.rgba8.as_chunks::<4>().0.iter())
    {
        let over = pixel
            .iter()
            .zip(reference.iter())
            .any(|(actual, golden)| actual.abs_diff(*golden) > tolerance.per_channel);
        if over {
            pixel.copy_from_slice(&[255, 0, 255, 255]);
        } else {
            for channel in pixel.iter_mut().take(3) {
                *channel /= 2;
            }
            pixel[3] = 255;
        }
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/golden-diff")
        .join(format!("{name}.png"));
    SnapshotImage {
        width: actual.width,
        height: actual.height,
        rgba8: rgba,
    }
    .save_png(&path)?;
    Ok(path)
}

fn root_of(app: &TestApp) -> NodeId {
    let window = app.window_id();
    app.app()
        .window(window)
        .expect("TestApp's window must exist")
        .tree()
        .root()
}

fn box_node(app: &mut TestApp, parent: NodeId, id: &str, rect: LogicalRect, paint: PaintSpec) {
    let window = app.window_id();
    let _ = NodeBuilder::boxed()
        .layout_override(rect)
        .paint(paint)
        .id(id)
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            parent,
        )
        .expect("scene node");
}

fn style_node(
    app: &mut TestApp,
    parent: NodeId,
    id: &str,
    style: Style,
    paint: PaintSpec,
) -> NodeId {
    let window = app.window_id();
    NodeBuilder::boxed()
        .style(style)
        .paint(paint)
        .id(id)
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            parent,
        )
        .expect("scene container")
}

fn abs_style(rect: LogicalRect, clip: Clip) -> Style {
    let mut style = StyleBuilder::new()
        .absolute()
        .left(rect.origin.x)
        .top(rect.origin.y)
        .size(rect.size.width, rect.size.height)
        .build();
    style.clip = clip;
    style
}

fn build_backdrop(app: &mut TestApp, root: NodeId, palette: &Palette) {
    box_node(
        app,
        root,
        "scene.backdrop",
        LogicalRect::new(px(0.0), px(0.0), px(240.0), px(160.0)),
        PaintSpec {
            background: palette.backdrop,
            ..PaintSpec::default()
        },
    );
}

fn build_shapes(app: &mut TestApp, root: NodeId, palette: &Palette) {
    build_backdrop(app, root, palette);
    box_node(
        app,
        root,
        "shapes.rect",
        LogicalRect::new(px(8.0), px(8.0), px(100.0), px(60.0)),
        PaintSpec {
            background: palette.accent,
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "shapes.radii",
        LogicalRect::new(px(120.0), px(8.0), px(100.0), px(60.0)),
        PaintSpec {
            background: palette.surface,
            corner_radius: [px(0.0), px(4.0), px(12.0), px(24.0)],
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "shapes.hairline",
        LogicalRect::new(px(8.0), px(78.0), px(100.0), px(60.0)),
        PaintSpec {
            background: palette.surface,
            border: Some(Border {
                width: px(1.0),
                color: palette.accent,
            }),
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "shapes.rounded_border",
        LogicalRect::new(px(120.0), px(78.0), px(100.0), px(60.0)),
        PaintSpec {
            background: palette.surface,
            corner_radius: [px(12.0); 4],
            border: Some(Border {
                width: px(2.0),
                color: palette.ink,
            }),
            ..PaintSpec::default()
        },
    );
}

fn build_shadow(app: &mut TestApp, root: NodeId, palette: &Palette) {
    build_backdrop(app, root, palette);
    box_node(
        app,
        root,
        "shadow.tight",
        LogicalRect::new(px(40.0), px(30.0), px(120.0), px(50.0)),
        PaintSpec {
            background: palette.surface,
            shadows: Shadows::from(ShadowSpec {
                offset: crate::math::LogicalPoint::new(px(2.0), px(4.0)),
                blur: px(4.0),
                spread: px(0.0),
                color: palette.ink,
            }),
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "shadow.wide",
        LogicalRect::new(px(70.0), px(95.0), px(120.0), px(40.0)),
        PaintSpec {
            background: palette.surface,
            corner_radius: [px(8.0); 4],
            shadows: Shadows::from(ShadowSpec {
                offset: crate::math::LogicalPoint::new(px(0.0), px(6.0)),
                blur: px(16.0),
                spread: px(0.0),
                color: palette.muted,
            }),
            ..PaintSpec::default()
        },
    );
}

fn build_gradient(app: &mut TestApp, root: NodeId, palette: &Palette) {
    build_backdrop(app, root, palette);
    let stops = [
        (
            GradientKind::Horizontal,
            LogicalRect::new(px(8.0), px(8.0), px(224.0), px(42.0)),
        ),
        (
            GradientKind::Vertical,
            LogicalRect::new(px(8.0), px(58.0), px(224.0), px(42.0)),
        ),
        (
            GradientKind::Radial,
            LogicalRect::new(px(8.0), px(108.0), px(224.0), px(44.0)),
        ),
    ];
    for (index, (kind, rect)) in stops.into_iter().enumerate() {
        box_node(
            app,
            root,
            match index {
                0 => "gradient.horizontal",
                1 => "gradient.vertical",
                _ => "gradient.radial",
            },
            rect,
            PaintSpec {
                background: Color::TRANSPARENT,
                gradient: Some(Gradient {
                    from: Color::Oklch {
                        l: 0.72,
                        c: 0.28,
                        h: 25.0,
                        alpha: 1.0,
                    },
                    to: Color::Oklab {
                        l: 0.45,
                        a: -0.12,
                        b: -0.18,
                        alpha: 1.0,
                    },
                    kind,
                }),
                ..PaintSpec::default()
            },
        );
    }
    box_node(
        app,
        root,
        "gradient.flat",
        LogicalRect::new(px(8.0), px(8.0), px(8.0), px(8.0)),
        PaintSpec {
            background: palette.accent,
            ..PaintSpec::default()
        },
    );
}

fn build_colors(app: &mut TestApp, root: NodeId, palette: &Palette) {
    box_node(
        app,
        root,
        "colors.backdrop",
        LogicalRect::new(px(0.0), px(0.0), px(160.0), px(120.0)),
        PaintSpec {
            background: palette.backdrop,
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "colors.stack.red",
        LogicalRect::new(px(12.0), px(12.0), px(60.0), px(60.0)),
        PaintSpec {
            background: Color::Srgb {
                r: 0.95,
                g: 0.2,
                b: 0.25,
                alpha: 0.55,
            },
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "colors.stack.blue",
        LogicalRect::new(px(42.0), px(32.0), px(60.0), px(60.0)),
        PaintSpec {
            background: Color::Srgb {
                r: 0.1,
                g: 0.35,
                b: 0.95,
                alpha: 0.45,
            },
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "colors.stack.white",
        LogicalRect::new(px(72.0), px(52.0), px(60.0), px(60.0)),
        PaintSpec {
            background: Color::Srgb {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                alpha: 0.3,
            },
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "colors.oklch",
        LogicalRect::new(px(12.0), px(84.0), px(60.0), px(24.0)),
        PaintSpec {
            background: Color::Oklch {
                l: 0.6,
                c: 0.36,
                h: 145.0,
                alpha: 1.0,
            },
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "colors.oklch_edge",
        LogicalRect::new(px(88.0), px(84.0), px(60.0), px(24.0)),
        PaintSpec {
            background: Color::Oklch {
                l: 0.9,
                c: 0.29,
                h: 300.0,
                alpha: 1.0,
            },
            ..PaintSpec::default()
        },
    );
}

fn build_image(app: &mut TestApp, root: NodeId, palette: &Palette) {
    box_node(
        app,
        root,
        "image.backdrop",
        LogicalRect::new(px(0.0), px(0.0), px(160.0), px(120.0)),
        PaintSpec {
            background: palette.backdrop,
            ..PaintSpec::default()
        },
    );
    let window = app.window_id();
    let icon = app
        .app_mut()
        .insert_bitmap_for_window(window, &snapshot_png())
        .expect("the synthetic 16x16 png must decode");
    let slots = [
        LogicalRect::new(px(16.0), px(16.0), px(16.0), px(16.0)),
        LogicalRect::new(px(48.0), px(16.0), px(32.0), px(32.0)),
        LogicalRect::new(px(16.0), px(56.0), px(16.0), px(16.0)),
    ];
    let modulations = [
        Color::WHITE,
        Color::WHITE,
        Color::Srgb {
            r: 1.0,
            g: 0.35,
            b: 0.2,
            alpha: 0.6,
        },
    ];
    for (index, (bounds, modulation)) in slots.into_iter().zip(modulations).enumerate() {
        let tile = app
            .app_mut()
            .prepare_image_variant(window, icon, bounds.size, ImageFit::Fill)
            .expect("the native bitmap variant is always available");
        let mut canvas = Canvas::new(bounds);
        canvas.push_image(Image {
            bounds,
            tile,
            modulation,
        });
        let _ = NodeBuilder::canvas(canvas)
            .layout_override(bounds)
            .id(match index {
                0 => "image.one_to_one",
                1 => "image.double",
                _ => "image.modulated",
            })
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("image node");
    }
}

fn build_clip(app: &mut TestApp, root: NodeId, palette: &Palette) {
    build_backdrop(app, root, palette);
    let outer = style_node(
        app,
        root,
        "clip.outer",
        abs_style(
            LogicalRect::new(px(8.0), px(8.0), px(140.0), px(90.0)),
            Clip::Bounds,
        ),
        PaintSpec {
            background: palette.surface,
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        outer,
        "clip.outer.overflow",
        LogicalRect::new(px(100.0), px(50.0), px(100.0), px(80.0)),
        PaintSpec {
            background: palette.accent,
            ..PaintSpec::default()
        },
    );
    let inner = style_node(
        app,
        outer,
        "clip.inner",
        abs_style(
            LogicalRect::new(px(16.0), px(16.0), px(80.0), px(50.0)),
            Clip::Bounds,
        ),
        PaintSpec {
            background: palette.muted,
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        inner,
        "clip.inner.overflow",
        LogicalRect::new(px(24.0), px(24.0), px(120.0), px(70.0)),
        PaintSpec {
            background: palette.ink,
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        outer,
        "clip.outer.free",
        LogicalRect::new(px(130.0), px(10.0), px(40.0), px(20.0)),
        PaintSpec {
            background: palette.ink,
            ..PaintSpec::default()
        },
    );
}

fn build_clip_subpixel(app: &mut TestApp, root: NodeId, palette: &Palette) {
    build_backdrop(app, root, palette);
    let outer = style_node(
        app,
        root,
        "clip.subpixel.outer",
        abs_style(
            LogicalRect::new(px(24.25), px(20.5), px(132.25), px(76.25)),
            Clip::Bounds,
        ),
        PaintSpec::default(),
    );
    box_node(
        app,
        outer,
        "clip.subpixel.overflow",
        LogicalRect::new(px(-40.0), px(-30.0), px(400.0), px(300.0)),
        PaintSpec {
            background: palette.ink,
            ..PaintSpec::default()
        },
    );
}

fn snapshot_png() -> Vec<u8> {
    let mut pixels = Vec::with_capacity(16 * 16 * 4);
    for y in 0..16u32 {
        for x in 0..16u32 {
            let rgba = match (x < 8, y < 8) {
                (true, true) => [230u8, 80, 60, 255],
                (false, true) => [240, 190, 70, 170],
                (true, false) => [70, 160, 230, 85],
                (false, false) => [40, 40, 40, 0],
            };
            pixels.extend_from_slice(&rgba);
        }
    }
    let image = image::RgbaImage::from_raw(16, 16, pixels).expect("16x16 buffer");
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use smol_str::SmolStr;

    use super::{
        GoldenError, SnapshotCase, SnapshotDiff, SnapshotImage, SnapshotScene, SnapshotTheme,
        Tolerance, compare, scale_token,
    };
    use crate::math::{DevicePixelRatio, LogicalSize, px};

    fn image(width: u32, height: u32, fill: [u8; 4]) -> SnapshotImage {
        SnapshotImage {
            width,
            height,
            rgba8: fill.repeat((width * height) as usize),
        }
    }

    fn solid_case(scene: SnapshotScene, theme: SnapshotTheme, scale: f32) -> SnapshotCase {
        SnapshotCase {
            scene,
            theme,
            size: LogicalSize::new(px(10.0), px(10.0)),
            scale: DevicePixelRatio::new(scale).expect("scale"),
        }
    }

    #[test]
    fn golden_name_uses_trimmed_scale() {
        assert_eq!(scale_token(1.0), "1");
        assert_eq!(scale_token(1.25), "1.25");
        assert_eq!(scale_token(2.0), "2");
        assert_eq!(scale_token(1.5), "1.5");
        assert_eq!(
            solid_case(SnapshotScene::Shapes, SnapshotTheme::Dark, 1.25).golden_name(),
            "shapes_1.25_dark"
        );
    }

    #[test]
    fn golden_path_is_under_the_device_directory() {
        let path = solid_case(SnapshotScene::Clip, SnapshotTheme::Light, 2.0).golden_path();
        let expected = format!("tests/golden/{}/clip_2_light.png", super::device_dir());
        assert!(path.ends_with(Path::new(&expected)), "{path:?}");
    }

    #[test]
    fn golden_path_is_unique_per_case() {
        let mut names = Vec::new();
        for scene in SnapshotScene::ALL {
            for theme in [SnapshotTheme::Light, SnapshotTheme::Dark] {
                for scale in [1.0f32, 1.25, 2.0] {
                    names.push(solid_case(scene, theme, scale).golden_name().to_string());
                }
            }
        }
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count, "golden 文件名必须唯一");
    }

    #[test]
    fn compare_identical_images_is_zero() {
        let actual = image(3, 2, [10, 20, 30, 40]);
        let diff = compare(&actual, &actual, Tolerance::default()).expect("compare");
        assert_eq!(
            diff,
            SnapshotDiff {
                max_channel_diff: 0,
                avg_channel_diff: 0.0,
                mismatched_pixels: 0,
                compared_pixels: 6,
                first_mismatch: None,
                mismatch_bounds: None,
            }
        );
    }

    #[test]
    fn compare_counts_mismatched_pixels() {
        let actual = image(2, 2, [100, 100, 100, 255]);
        let mut golden = actual.clone();
        golden.rgba8[0] = 103;
        golden.rgba8[5] = 102;

        let diff = compare(&actual, &golden, Tolerance::default()).expect("compare");
        assert_eq!(diff.mismatched_pixels, 1);
        assert_eq!(diff.max_channel_diff, 3);
        assert_eq!(diff.first_mismatch, Some((0, 0)));

        let exact = compare(&actual, &golden, Tolerance::EXACT).expect("compare");
        assert_eq!(exact.mismatched_pixels, 2, "EXACT 容差下两处都不匹配");
    }

    #[test]
    fn compare_reports_the_mismatch_bounds() {
        let actual = image(3, 2, [0, 0, 0, 255]);
        let mut golden = actual.clone();
        golden.rgba8[0] = 9;
        golden.rgba8[8] = 9;
        golden.rgba8[17] = 9;

        let diff = compare(&actual, &golden, Tolerance::default()).expect("compare");
        assert_eq!(diff.mismatched_pixels, 3);
        assert_eq!(diff.first_mismatch, Some((0, 0)));
        assert_eq!(
            diff.mismatch_bounds,
            Some(super::PixelBounds {
                min_x: 0,
                min_y: 0,
                max_x: 2,
                max_y: 1,
            })
        );
        assert_eq!(
            diff.mismatch_bounds.map(|bounds| bounds.to_string()),
            Some("(0,0)-(2,1)".to_string())
        );
    }

    #[test]
    fn compare_reports_max_and_avg() {
        let actual = image(1, 1, [0, 0, 0, 0]);
        let golden = SnapshotImage {
            width: 1,
            height: 1,
            rgba8: vec![4, 8, 12, 16],
        };
        let diff = compare(&actual, &golden, Tolerance::default()).expect("compare");
        assert_eq!(diff.max_channel_diff, 16);
        assert!((diff.avg_channel_diff - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn compare_records_first_divergent_pixel() {
        let actual = image(3, 1, [0, 0, 0, 255]);
        let mut golden = actual.clone();
        golden.rgba8[8] = 200;

        let diff = compare(&actual, &golden, Tolerance::default()).expect("compare");
        assert_eq!(diff.first_mismatch, Some((2, 0)));
    }

    #[test]
    fn compare_rejects_dimension_mismatch() {
        let actual = image(2, 2, [0; 4]);
        let golden = image(2, 3, [0; 4]);
        assert_eq!(
            compare(&actual, &golden, Tolerance::default()),
            Err(GoldenError::Dimensions {
                actual: (2, 2),
                golden: (2, 3)
            })
        );
    }

    #[test]
    fn compare_rejects_a_malformed_buffer_in_every_profile() {
        let mut actual = image(2, 2, [0; 4]);
        actual.rgba8.truncate(12);
        assert!(matches!(
            compare(&actual, &image(2, 2, [0; 4]), Tolerance::default()),
            Err(GoldenError::Malformed { .. })
        ));

        let mut golden = image(2, 2, [0; 4]);
        golden.rgba8.push(0);
        assert!(matches!(
            compare(&image(2, 2, [0; 4]), &golden, Tolerance::default()),
            Err(GoldenError::Malformed { .. })
        ));
    }

    #[test]
    fn diff_error_reports_count_max_first_and_bounds() {
        let error = GoldenError::Diff {
            case: SmolStr::new_static("shapes_1_light"),
            diff: SnapshotDiff {
                max_channel_diff: 9,
                avg_channel_diff: 0.5,
                mismatched_pixels: 3,
                compared_pixels: 4,
                first_mismatch: Some((1, 0)),
                mismatch_bounds: Some(super::PixelBounds {
                    min_x: 1,
                    min_y: 0,
                    max_x: 1,
                    max_y: 1,
                }),
            },
            allowed: 0,
            golden: PathBuf::from("golden.png"),
            heatmap: PathBuf::from("heatmap.png"),
        };
        let text = error.to_string();
        assert!(text.contains("3 mismatched of 4 pixels"), "{text}");
        assert!(text.contains("allowed 0"), "{text}");
        assert!(text.contains("max channel diff 9"), "{text}");
        assert!(text.contains("first mismatch (1,0)"), "{text}");
        assert!(text.contains("mismatch bounds (1,0)-(1,1)"), "{text}");
        assert!(text.contains("golden.png"), "{text}");
        assert!(text.contains("heatmap.png"), "{text}");
    }

    #[test]
    fn tolerance_default_is_the_recorded_value() {
        let tolerance = Tolerance::default();
        assert_eq!(tolerance.per_channel, 2);
        assert_eq!(tolerance.max_mismatch_ppm, 1_000, "0.1%");
        assert_eq!(Tolerance::GEOMETRY.per_channel, 2);
        assert_eq!(Tolerance::GEOMETRY.max_mismatch_ppm, 0);
        assert_eq!(Tolerance::GEOMETRY.allowed_mismatches(38_400), 0);
    }

    #[test]
    fn tolerance_allows_a_ratio_of_mismatched_pixels() {
        let tolerance = Tolerance::default();
        assert_eq!(tolerance.allowed_mismatches(38_400), 38);
        assert_eq!(tolerance.allowed_mismatches(1_000), 1);
        assert_eq!(tolerance.allowed_mismatches(4), 0);
        assert_eq!(Tolerance::EXACT.allowed_mismatches(38_400), 0);
    }

    #[test]
    fn snapshot_case_is_copy_and_debug() {
        fn assert_copy<T: Copy + std::fmt::Debug>() {}
        assert_copy::<SnapshotCase>();
        assert_copy::<SnapshotScene>();
        assert_copy::<SnapshotTheme>();
        let case = SnapshotCase::default();
        assert_eq!(case.scene, SnapshotScene::Shapes);
        assert_eq!(case.theme, SnapshotTheme::Light);
    }

    #[test]
    fn snapshot_png_is_a_valid_png() {
        let bytes = super::snapshot_png();
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        let decoded = image::load_from_memory(&bytes).expect("png");
        assert_eq!((decoded.width(), decoded.height()), (16, 16));
    }

    #[test]
    fn device_dir_matches_the_platform_backend() {
        let expected = if cfg!(windows) {
            "warp"
        } else if cfg!(target_os = "macos") {
            "metal-real"
        } else {
            "lavapipe"
        };
        assert_eq!(super::device_dir(), expected);
        let case = SnapshotCase::default();
        assert!(
            case.golden_path().starts_with(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/golden")
                    .join(expected)
            ),
            "{:?}",
            case.golden_path()
        );
    }
}

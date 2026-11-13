//! 像素快照（L3）：读回 → 差图统计 → golden 比对（P13 §9）。
//!
//! 语义：`TestApp::snapshot` 只读**最近一次提交**的实例缓冲（P08 §7.7），因此
//! [`capture`] 的顺序是「建 `TestApp` → [`mount`] 合成场景 → `drain()` → `snapshot`」。
//! `SnapshotCase` 的 `scene` / `theme` 只决定「挂哪个场景」与「golden 文件名」，不参与渲染本身。
//!
//! 平台策略（§9.6 / D14）：golden 按**设备类型**分目录；三个平台各自产出并与自己的基线比对
//! （跨设备比同一份 golden 会永久红）。`golden` 的字节是读回原样（预乘 sRGB RGBA8），不反预乘。

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use smol_str::{SmolStr, format_smolstr};

use crate::{
    math::{Rect, Scale, Size},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{Border, Gradient, GradientKind, Image},
        style::{Clip, PaintSpec, ShadowSpec, Style},
    },
    test_support::app::TestApp,
    window::WindowOptions,
};

/// 一次快照请求（合成场景 + 分辨率 + scale + 主题）。
///
/// `scene` / `theme` **不参与渲染**：它们只决定 golden 的文件名与「挂哪个合成场景」。
/// 渲染仍然是「读最近一次提交的实例缓冲」（P08 §7.7 的既有语义），因此 `TestApp::snapshot`
/// 只消费 `size` / `scale`。场景由 [`mount`] 在 `drain()` 之前挂载。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotCase {
    /// 合成场景。
    pub scene: SnapshotScene,
    /// 明暗配色（引擎里只是场景标签；主题语义在应用层）。
    pub theme: SnapshotTheme,
    /// 逻辑尺寸。
    pub size: Size,
    /// 设备比例。
    pub scale: Scale,
}

impl Default for SnapshotCase {
    fn default() -> Self {
        Self {
            scene: SnapshotScene::Shapes,
            theme: SnapshotTheme::Light,
            size: Size::new(800.0, 600.0),
            scale: Scale::ONE,
        }
    }
}

impl SnapshotCase {
    /// golden 文件名主干：`"<scene>_<scale>_<theme>"`（例：`shapes_1.25_dark`）。
    #[must_use]
    pub fn golden_name(&self) -> SmolStr {
        format_smolstr!(
            "{}_{}_{}",
            self.scene.token(),
            scale_token(self.scale.factor()),
            self.theme.token()
        )
    }

    /// `CARGO_MANIFEST_DIR/tests/golden/<device>/<golden_name>.png`。
    ///
    /// 按**设备类型**分目录（D14）：`warp/`（Windows WARP）、`lavapipe/`（Linux Mesa llvmpipe）。
    /// 软件光栅与真实 GPU 的 AA 存在系统性差异，跨设备比同一份 golden 会永久红。
    #[must_use]
    pub fn golden_path(&self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(device_dir())
            .join(format!("{}.png", self.golden_name()))
    }
}

/// 当前平台的 golden 目录（D14 的设备分组）。
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

/// 合成场景（引擎内建，**不依赖 `base_ui`**：集成测试不能反向依赖应用层）。
///
/// 没有 `Glyphs` 变体：文字像素依赖宿主字体，不能进 v1 golden（F-144 / V-96）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnapshotScene {
    /// 矩形 / 四角不同圆角 / 1px 边框。
    Shapes,
    /// 解析阴影（blur 4 与 16）。
    Shadow,
    /// 线性水平 / 线性垂直 / 径向渐变。
    Gradient,
    /// 纯色 / 半透明叠加 / 高彩度 OKLCH（gamut mapping 边界）。
    Colors,
    /// 图片图集：1:1 / 2× 放大 / 半透明调制（四象限 alpha = 255/170/85/0）。
    Image,
    /// 嵌套 clip + 溢出子矩形。
    Clip,
}

impl SnapshotScene {
    /// 全部场景（golden 唯一性断言用）。
    pub const ALL: [Self; 6] = [
        Self::Shapes,
        Self::Shadow,
        Self::Gradient,
        Self::Colors,
        Self::Image,
        Self::Clip,
    ];

    /// 文件名用的稳定 token。
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Shapes => "shapes",
            Self::Shadow => "shadow",
            Self::Gradient => "gradient",
            Self::Colors => "colors",
            Self::Image => "image",
            Self::Clip => "clip",
        }
    }

    /// 场景的逻辑尺寸（与 golden case 表一致）。
    #[must_use]
    pub fn size(self) -> Size {
        match self {
            Self::Shapes | Self::Shadow | Self::Gradient | Self::Clip => Size::new(240.0, 160.0),
            Self::Colors | Self::Image => Size::new(160.0, 120.0),
        }
    }

    /// 挂载到 `app` 当前窗口的根节点下（调用方负责先 `drain()` 再 `snapshot`）。
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
        }
    }
}

/// 明暗「主题」在引擎里只是**合成场景的配色标签**（D15：主题语义在应用层）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnapshotTheme {
    /// 亮底深字。
    Light,
    /// 深底亮字（也用来钉「预乘叠加在暗底上的表现」）。
    Dark,
}

impl SnapshotTheme {
    /// 文件名用的稳定 token。
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// 场景配色（只服务 golden，不进引擎）。
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

/// 合成场景的配色（引擎无主题概念）。
struct Palette {
    backdrop: Color,
    surface: Color,
    ink: Color,
    accent: Color,
    muted: Color,
}

/// 读回结果：**预乘 sRGB 编码**的 RGBA8（与 `render/*/readback.rs` 的约定一致）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotImage {
    /// 像素宽。
    pub width: u32,
    /// 像素高。
    pub height: u32,
    /// `width * height * 4` 字节。
    pub rgba8: Vec<u8>,
}

impl SnapshotImage {
    /// 写 PNG（golden 入库用；逐字节无损）。
    ///
    /// # Errors
    /// [`GoldenError::Io`]（父目录不可建 / 写失败）或 [`GoldenError::GoldenDecode`]（尺寸与缓冲不符）。
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

    /// 读 PNG（golden 比对用）。
    ///
    /// # Errors
    /// [`GoldenError::Io`]（文件不存在 / 读失败）或 [`GoldenError::GoldenDecode`]（不是可解码的 PNG）。
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

/// 快照失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// 本构建没有渲染器（headless 的默认）。
    NoRenderer,
    /// GPU 路径失败（设备创建 / 渲染 / 读回）。
    Gpu {
        /// 可读的诊断（含 HRESULT 或具体阶段）。
        detail: SmolStr,
    },
}

/// 逐通道容差（D19：**不冻结数值**，见 `design/phase-13-resources-snapshot.md` §9.5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tolerance {
    /// 单通道允许的最大绝对差（`0..=255`）。
    pub per_channel: u8,
    /// 允许「有任一通道超出 `per_channel`」的**像素数**上限。
    pub max_over_tolerance_pixels: u32,
}

impl Tolerance {
    /// 逐字节相等。
    pub const EXACT: Self = Self {
        per_channel: 0,
        max_over_tolerance_pixels: 0,
    };
}

impl Default for Tolerance {
    fn default() -> Self {
        // P08 §7.7 的提案值；校正属 L3/P7（D19 不预设数值）。
        Self {
            per_channel: 1,
            max_over_tolerance_pixels: 0,
        }
    }
}

/// 一次比较的统计。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotDiff {
    /// 全图最大单通道差（`0..=255`）。
    pub max_channel_diff: u8,
    /// 全图所有通道差的均值（`0..=255` 标度）。
    pub avg_channel_diff: f32,
    /// 有任一通道差 `> tolerance.per_channel` 的像素数。
    pub over_tolerance_pixels: u32,
    /// 参与比较的像素数（`width * height`）。
    pub compared_pixels: u32,
    /// 第一个超容差像素的坐标 `(x, y)`。
    pub first_over_tolerance: Option<(u32, u32)>,
}

/// golden 层错误（**不** `derive(Eq)`：`Diff.diff` 含 `f32`）。
#[derive(Clone, Debug, PartialEq)]
pub enum GoldenError {
    /// 读回失败（设备 / 渲染）。
    Snapshot(SnapshotError),
    /// 场景推进失败（文本塑形 / 图集耗尽等）。
    Frame {
        /// `EngineError` 的英文诊断文本。
        detail: SmolStr,
    },
    /// 读回尺寸与 golden 不同。
    Dimensions {
        /// 实际尺寸。
        actual: (u32, u32),
        /// golden 尺寸。
        golden: (u32, u32),
    },
    /// 读/写 golden 失败（含「golden 不存在」）。
    Io {
        /// 出错路径。
        path: PathBuf,
        /// 诊断文本。
        detail: SmolStr,
    },
    /// golden 文件不是可解码的 PNG / 尺寸不自洽。
    GoldenDecode {
        /// 出错路径。
        path: PathBuf,
        /// 诊断文本。
        detail: SmolStr,
    },
    /// 输入图像自相矛盾（`rgba8` 长度与 `width × height × 4` 不符）。
    ///
    /// 显式错误而不是 `debug_assert`：release 下 `debug_assert` 会被编掉，长度不符会**静默少比**。
    Malformed {
        /// 诊断文本。
        detail: SmolStr,
    },
    /// 超容差（含统计与热力图路径）。
    Diff {
        /// golden 文件名主干。
        case: SmolStr,
        /// 统计。
        diff: SnapshotDiff,
        /// golden 路径。
        golden: PathBuf,
        /// 失败热力图路径（`target/golden-diff/`）。
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
                golden,
                heatmap,
            } => write!(
                f,
                "golden `{case}` differs: max={} avg={:.3} over={} (golden {}, heatmap {})",
                diff.max_channel_diff,
                diff.avg_channel_diff,
                diff.over_tolerance_pixels,
                golden.display(),
                heatmap.display()
            ),
        }
    }
}

/// 比较两张读回图（尺寸必须一致，缓冲长度必须自洽）。
///
/// # Errors
/// [`GoldenError::Dimensions`]（宽高不等）或 [`GoldenError::Malformed`]（缓冲长度与宽高不符）。
///
/// 长度检查**不是** `debug_assert`：release 下 `debug_assert` 会被编掉，`zip` 会静默少比。
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
    let mut over_tolerance_pixels = 0u32;
    let mut first_over_tolerance = None;
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
            over_tolerance_pixels += 1;
            if first_over_tolerance.is_none() {
                let x = (index % actual.width as usize) as u32;
                let y = (index / actual.width as usize) as u32;
                first_over_tolerance = Some((x, y));
            }
        }
    }
    let channels = (pixels * 4).max(1) as f32;
    Ok(SnapshotDiff {
        max_channel_diff,
        avg_channel_diff: sum as f32 / channels,
        over_tolerance_pixels,
        compared_pixels: pixels as u32,
        first_over_tolerance,
    })
}

/// 在 `app` 当前窗口的根节点下挂载合成场景（调用方随后 `drain()`）。
pub fn mount(app: &mut TestApp, scene: SnapshotScene, theme: SnapshotTheme) {
    scene.build(app, theme);
}

/// 冷启动一个 case：建 `TestApp` → 挂场景 → `drain` → `snapshot`。
///
/// # Errors
/// [`GoldenError::Frame`]（场景推进失败）或 [`GoldenError::Snapshot`]（无渲染器 / GPU 失败）。
pub fn capture(case: SnapshotCase) -> Result<SnapshotImage, GoldenError> {
    let mut app = TestApp::with_options(WindowOptions {
        title: SmolStr::new_static("kytrix golden"),
        size: case.size,
        scale: case.scale,
    });
    mount(&mut app, case.scene, case.theme);
    app.drain().map_err(|error| GoldenError::Frame {
        detail: SmolStr::new(error.to_string()),
    })?;
    app.snapshot(case).map_err(GoldenError::Snapshot)
}

/// 覆盖写 golden（父目录不存在时创建）。
///
/// # Errors
/// [`GoldenError::Io`] / [`GoldenError::GoldenDecode`]（见 [`SnapshotImage::save_png`]）。
pub fn write_golden(case: SnapshotCase, image: &SnapshotImage) -> Result<(), GoldenError> {
    image.save_png(&case.golden_path())
}

/// 比较一个 case（`UPDATE_GOLDEN` 存在时先写 golden，再读回比较）。
///
/// # Errors
/// [`GoldenError`]：`Snapshot` / `Frame` / `Io`（缺 golden）/ `Dimensions` / `Diff`。
pub fn assert_golden(
    case: SnapshotCase,
    tolerance: Tolerance,
) -> Result<SnapshotDiff, GoldenError> {
    let actual = capture(case)?;
    if update_requested() {
        write_golden(case, &actual)?;
    }
    let path = case.golden_path();
    let golden = SnapshotImage::load_png(&path)?;
    let diff = compare(&actual, &golden, tolerance)?;
    if diff.over_tolerance_pixels > tolerance.max_over_tolerance_pixels {
        let heatmap = write_heatmap(&case.golden_name(), &actual, &golden, tolerance)?;
        return Err(GoldenError::Diff {
            case: case.golden_name(),
            diff,
            golden: path,
            heatmap,
        });
    }
    Ok(diff)
}

/// 进程内串行化 GPU 快照（多个 `#[test]` 并行会各自建 WARP 设备并互相拖垮）。
pub fn gpu_guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// `UPDATE_GOLDEN` 存在即写（不看值）。
fn update_requested() -> bool {
    std::env::var_os("UPDATE_GOLDEN").is_some()
}

/// 小数点后最多两位、去尾零（`1.0 → "1"`、`1.25 → "1.25"`、`2.0 → "2"`）。
fn scale_token(scale: f32) -> SmolStr {
    let text = format_smolstr!("{scale:.2}");
    SmolStr::new(text.trim_end_matches('0').trim_end_matches('.'))
}

/// 失败热力图：超容差像素涂洋红，其余取实际值的一半亮度；写入 `target/golden-diff/`。
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

/// 当前窗口的根节点。
fn root_of(app: &TestApp) -> NodeId {
    let window = app.window_id();
    app.app()
        .window(window)
        .expect("TestApp's window must exist")
        .tree()
        .root()
}

/// 建一个绝对定位的 box 节点（场景构建的唯一原语）。
fn box_node(app: &mut TestApp, parent: NodeId, id: &str, rect: Rect, paint: PaintSpec) {
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

/// 建一个带 `Style` 的容器（clip 场景用）。
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

/// 绝对定位的 `Style`（`size` + 可选 clip）。
fn abs_style(rect: Rect, clip: Clip) -> Style {
    let inner = taffy::Style::<SmolStr> {
        position: taffy::style::Position::Absolute,
        size: taffy::geometry::Size {
            width: taffy::style::Dimension::length(rect.size.width),
            height: taffy::style::Dimension::length(rect.size.height),
        },
        inset: taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(rect.origin.x),
            top: taffy::style::LengthPercentageAuto::length(rect.origin.y),
            right: taffy::style::LengthPercentageAuto::auto(),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        },
        ..taffy::Style::<SmolStr>::DEFAULT
    };
    Style { inner, clip }
}

/// 铺满场景的底色（所有场景共用：让 golden 覆盖「背景填充 + 主题色」而不是全透明区）。
fn build_backdrop(app: &mut TestApp, root: NodeId, palette: &Palette) {
    box_node(
        app,
        root,
        "scene.backdrop",
        Rect::new(0.0, 0.0, 240.0, 160.0),
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
        Rect::new(8.0, 8.0, 100.0, 60.0),
        PaintSpec {
            background: palette.accent,
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "shapes.radii",
        Rect::new(120.0, 8.0, 100.0, 60.0),
        PaintSpec {
            background: palette.surface,
            corner_radius: [0.0, 4.0, 12.0, 24.0],
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "shapes.hairline",
        Rect::new(8.0, 78.0, 100.0, 60.0),
        PaintSpec {
            background: palette.surface,
            border: Some(Border {
                width: 1.0,
                color: palette.accent,
            }),
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "shapes.rounded_border",
        Rect::new(120.0, 78.0, 100.0, 60.0),
        PaintSpec {
            background: palette.surface,
            corner_radius: [12.0; 4],
            border: Some(Border {
                width: 2.0,
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
        Rect::new(40.0, 30.0, 120.0, 50.0),
        PaintSpec {
            background: palette.surface,
            shadow: Some(ShadowSpec {
                offset: crate::math::Point::new(2.0, 4.0),
                blur: 4.0,
                color: palette.ink,
            }),
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "shadow.wide",
        Rect::new(70.0, 95.0, 120.0, 40.0),
        PaintSpec {
            background: palette.surface,
            corner_radius: [8.0; 4],
            shadow: Some(ShadowSpec {
                offset: crate::math::Point::new(0.0, 6.0),
                blur: 16.0,
                color: palette.muted,
            }),
            ..PaintSpec::default()
        },
    );
}

fn build_gradient(app: &mut TestApp, root: NodeId, palette: &Palette) {
    build_backdrop(app, root, palette);
    let stops = [
        (GradientKind::Horizontal, Rect::new(8.0, 8.0, 224.0, 42.0)),
        (GradientKind::Vertical, Rect::new(8.0, 58.0, 224.0, 42.0)),
        (GradientKind::Radial, Rect::new(8.0, 108.0, 224.0, 44.0)),
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
                    // 高彩度端点：覆盖 sRGB gamut mapping（P02 §5.4）。
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
    // 与渐变并排的一格实色（对照：确认渐变路径不影响普通 Quad）。
    box_node(
        app,
        root,
        "gradient.flat",
        Rect::new(8.0, 8.0, 8.0, 8.0),
        PaintSpec {
            background: palette.accent,
            ..PaintSpec::default()
        },
    );
}

fn build_colors(app: &mut TestApp, root: NodeId, palette: &Palette) {
    // 底色（主题相关）+ 三块半透明叠色 + 一块高彩度 OKLCH（gamut 边界）。
    box_node(
        app,
        root,
        "colors.backdrop",
        Rect::new(0.0, 0.0, 160.0, 120.0),
        PaintSpec {
            background: palette.backdrop,
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        root,
        "colors.stack.red",
        Rect::new(12.0, 12.0, 60.0, 60.0),
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
        Rect::new(42.0, 32.0, 60.0, 60.0),
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
        Rect::new(72.0, 52.0, 60.0, 60.0),
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
    // 高彩度 OKLCH：C 超出 sRGB gamut，走 CSS Color 4 的 chroma 归零映射。
    box_node(
        app,
        root,
        "colors.oklch",
        Rect::new(12.0, 84.0, 60.0, 24.0),
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
        Rect::new(88.0, 84.0, 60.0, 24.0),
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
        Rect::new(0.0, 0.0, 160.0, 120.0),
        PaintSpec {
            background: palette.backdrop,
            ..PaintSpec::default()
        },
    );
    // 四象限 alpha = 255 / 170 / 85 / 0（由 `image` 现编码，不落外部资产）。
    let icon = app
        .app_mut()
        .resources_mut()
        .insert(&snapshot_png())
        .expect("the synthetic 16x16 png must decode");
    let slots = [
        Rect::new(16.0, 16.0, 16.0, 16.0),
        Rect::new(48.0, 16.0, 32.0, 32.0),
        Rect::new(16.0, 56.0, 16.0, 16.0),
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
        let mut canvas = Canvas::new(bounds);
        canvas.push_image(Image {
            bounds,
            tile: icon.tile,
            modulation,
        });
        let window = app.window_id();
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
        abs_style(Rect::new(8.0, 8.0, 140.0, 90.0), Clip::Bounds),
        PaintSpec {
            background: palette.surface,
            ..PaintSpec::default()
        },
    );
    // 溢出子矩形：被外层裁掉。
    box_node(
        app,
        outer,
        "clip.outer.overflow",
        Rect::new(100.0, 50.0, 100.0, 80.0),
        PaintSpec {
            background: palette.accent,
            ..PaintSpec::default()
        },
    );
    let inner = style_node(
        app,
        outer,
        "clip.inner",
        abs_style(Rect::new(16.0, 16.0, 80.0, 50.0), Clip::Bounds),
        PaintSpec {
            background: palette.muted,
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        inner,
        "clip.inner.overflow",
        Rect::new(24.0, 24.0, 120.0, 70.0),
        PaintSpec {
            background: palette.ink,
            ..PaintSpec::default()
        },
    );
    box_node(
        app,
        outer,
        "clip.outer.free",
        Rect::new(130.0, 10.0, 40.0, 20.0),
        PaintSpec {
            background: palette.ink,
            ..PaintSpec::default()
        },
    );
}

/// 16×16 的合成 PNG（四象限 alpha = 255/170/85/0），供 `Image` 场景走真实解码链路。
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
    use std::path::Path;

    use super::{
        GoldenError, SnapshotCase, SnapshotDiff, SnapshotImage, SnapshotScene, SnapshotTheme,
        Tolerance, compare, scale_token,
    };
    use crate::math::{Scale, Size};

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
            size: Size::new(10.0, 10.0),
            scale: Scale::new(scale).expect("scale"),
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
                over_tolerance_pixels: 0,
                compared_pixels: 6,
                first_over_tolerance: None,
            }
        );
    }

    #[test]
    fn compare_counts_over_tolerance_pixels() {
        let actual = image(2, 2, [100, 100, 100, 255]);
        let mut golden = actual.clone();
        // 一个像素差 2（超过 per_channel = 1），另一个差 1（在容差内）。
        golden.rgba8[0] = 102;
        golden.rgba8[5] = 101;

        let diff = compare(&actual, &golden, Tolerance::default()).expect("compare");
        assert_eq!(diff.over_tolerance_pixels, 1);
        assert_eq!(diff.max_channel_diff, 2);
        assert_eq!(diff.first_over_tolerance, Some((0, 0)));

        let exact = compare(&actual, &golden, Tolerance::EXACT).expect("compare");
        assert_eq!(exact.over_tolerance_pixels, 2, "EXACT 容差下两处都算超差");
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
        golden.rgba8[8] = 200; // 第 3 个像素的第一个通道

        let diff = compare(&actual, &golden, Tolerance::default()).expect("compare");
        assert_eq!(diff.first_over_tolerance, Some((2, 0)));
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
        // release 下 `debug_assert` 会被编掉：长度不符必须是**显式错误**，不能静默少比。
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
    fn tolerance_default_is_the_recorded_value() {
        // D19：数值不冻结，但默认值必须与 design/phase-13 §9.5 的记录一致（改这里也要改文档）。
        let tolerance = Tolerance::default();
        assert_eq!(tolerance.per_channel, 1);
        assert_eq!(tolerance.max_over_tolerance_pixels, 0);
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

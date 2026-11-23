//! `base_ui` 的构建脚本：把 Phosphor SVG 图标光栅化成**白色字形 PNG**（写入 `OUT_DIR`）。
//!
//! 为什么要构建期做（用户裁决 + 设计 §8.5 / §5.6）：
//! - 引擎 `ResourceCache` 只收 PNG / JPEG / WebP，明确**不碰 usvg / resvg**（`resources.rs` 的三条
//!   硬规则之二）；SVG 必须在应用层、在构建期变成位图。
//! - 图标在运行期要能被主题色染色（旧实现是 `svg().text_color(..)`）。引擎的图片通道只有
//!   「白色字形 + `Primitive::Image.modulation`」这一条调制路径，所以这里把每种图标烘成
//!   **RGB 全白、alpha = 字形覆盖**的 PNG；运行期用 `Color` 调制即可得到任意颜色。
//!
//! 产物契约（`crates/base_ui/src/kit/icon.rs` 的 `include_bytes!` 依赖它，改名即编译失败）：
//! - `OUT_DIR/icons/<stem>-48.png` / `<stem>-96.png`：两档边长，供 1× / 2× 窗口选档；
//! - `OUT_DIR/icons/built.rs`：全部已生成图标的 stem（字母序），测试用它证明「资产 ⇄ 枚举」无漂移。
//!
//! # 像素契约：white matte（透明像素也是白）
//!
//! PNG 必须是直通 alpha，且**所有像素（包括 `alpha == 0` 的像素）的 RGB 都写白色**。这不是
//! 装饰性细节：引擎的图片管线是「straight-alpha 解码 → **先缩放** → 最后在**线性空间预乘**」
//! （`ui_core/src/resources/image.rs` 的顺序 5），而缩放用的 `image` 按通道独立插值、不做预乘
//! （其 `resize` 文档明确「assumes alpha pre-multiplication」）。若透明像素写 RGB=0，缩放时
//! 部分覆盖的边界像素会被拉向黑色 —— 实测 `check-circle-48 → 16` 最坏暗边 **96/255**：
//! 图标在所有常规尺寸下都是 3× 下采样，于是每个图标都会带一圈暗边。把透明像素的 RGB 也写成
//! 白色（white matte）后，RGB 通道是常量 255 ⇒ 插值结果恒 255，缩放不引入暗色，预乘后正是
//! 「白色字形 × 覆盖 alpha」应有的存储值。`kit::icon` 的两条测试锁定它：
//! `every_icon_raster_is_a_white_glyph_png_of_the_expected_tier`（产物全白）与
//! `downscaled_icon_edges_have_no_dark_fringe`（复刻引擎管线，断言下采样后无暗边）。
//!
//! 确定性：文件名与内容都只由输入 SVG 决定 —— 固定缩放算法（resvg 的解析 AA）、固定档位、无时间戳、
//! 无随机、无并发写；目录条目按文件名排序后处理，PNG 编码参数固定（`image` 的默认压缩/自适应过滤）。
//!
//! 失败语义：任何一步失败都 `panic!`（构建失败）并给出可读的「文件 + 原因 + 修复方向」，
//! 绝不静默产出缺失/空图标（I10：不静默降级）。

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use image::ImageEncoder;

/// 图标源目录（相对 crate 根）。
const ICON_SOURCE_DIR: &str = "assets/icons/phosphoricons";

/// 每个图标生成的两档边长（物理像素）。
///
/// 旧使用的图标尺寸只有 8 / 12 / 16 逻辑 px（`UnifiedSize::icon_size` 与 `.size_4()`，见
/// `.docs/gui-migration/spec-deltas/kit-assets.md` §2）：16 px @2× = 32 物理 px，48 档有 1.5×
/// 余量、96 档 3×，两档足够覆盖 1×/2× 且不至于让图集按 6× 无谓膨胀。
const RASTER_TIERS: [u32; 2] = [48, 96];

/// 源 SVG 里唯一的填色写法（55 个 Phosphor 文件全部是这一个；构建期替换为白色）。
const SOURCE_FILL: &str = "fill=\"#000000\"";

/// 替换成白色字形后的填色。
const WHITE_FILL: &str = "fill=\"#FFFFFF\"";

/// 完全透明像素的 RGB（white matte，见模块文档的像素契约）。
///
/// 必须与字形的白色一致：透明区的 RGB 不是一个「随便的占位值」，它参与引擎在 straight-alpha
/// 空间的缩放插值 —— 写 0 就是黑边，写 255 才是「透明处无颜色偏置」。
const MATTE_RGB: u8 = 0xFF;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir.join(ICON_SOURCE_DIR);
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("icons");

    println!("cargo:rerun-if-changed={}", source_dir.display());
    println!("cargo:rerun-if-changed=build.rs");

    let sources = icon_sources(&source_dir);
    fs::create_dir_all(&out_dir).unwrap_or_else(|error| {
        fail(&format!(
            "无法创建图标输出目录 `{}`：{error}\n  修复：检查 OUT_DIR 的可写权限",
            out_dir.display()
        ))
    });

    let mut built = Vec::with_capacity(sources.len());
    for (stem, path) in sources {
        let svg = fs::read_to_string(&path).unwrap_or_else(|error| {
            fail(&format!(
                "无法读取图标源 `{}`：{error}\n  修复：确认 `{ICON_SOURCE_DIR}` 下的 SVG 是 UTF-8 文本",
                path.display()
            ))
        });
        let svg = white_glyph_svg(&stem, &svg);
        let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).unwrap_or_else(|error| {
            fail(&format!(
                "图标 `{stem}` 的 SVG 解析失败：{error}\n  修复：Phosphor 图标必须是纯路径（本构建脚本不支持 \
                 文本 / 内嵌位图 / 外部引用）"
            ))
        });
        for tier in RASTER_TIERS {
            let png = rasterize(&stem, &tree, tier);
            let artifact = out_dir.join(format!("{stem}-{tier}.png"));
            fs::write(&artifact, &png).unwrap_or_else(|error| {
                fail(&format!(
                    "无法写入图标产物 `{}`（{} B）：{error}",
                    artifact.display(),
                    png.len()
                ))
            });
        }
        built.push(stem);
    }

    write_manifest(&out_dir, &built);
}

/// 目录里的 SVG（按文件名排序，保证「生成顺序 / 清单顺序」与文件系统枚举顺序无关）。
///
/// `assets/` 目录不存在、没有 SVG、或有非 `.svg` 文件时都直接失败：静默跳过一个文件就意味着
/// 某个图标在运行期变成空白，问题会推迟到用户可见的界面里才暴露。
fn icon_sources(source_dir: &Path) -> Vec<(String, PathBuf)> {
    let entries = fs::read_dir(source_dir).unwrap_or_else(|error| {
        fail(&format!(
            "无法读取图标源目录 `{}`：{error}\n  修复：`{ICON_SOURCE_DIR}` 必须随 crate 一起提交",
            source_dir.display()
        ))
    });
    let mut sources = Vec::new();
    for entry in entries {
        let path = entry
            .unwrap_or_else(|error| fail(&format!("无法枚举图标源目录：{error}")))
            .path();
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let Some(stem) = file_name.strip_suffix(".svg") else {
            fail(&format!(
                "图标源目录 `{}` 里有非 SVG 文件 `{file_name}`\n  修复：图标源目录只放 Phosphor \
                 `<name>.svg`（文档 / 许可证等放到别处）",
                source_dir.display()
            ));
        };
        sources.push((stem.to_owned(), path));
    }
    sources.sort_by(|left, right| left.0.cmp(&right.0));
    if sources.is_empty() {
        fail(&format!(
            "图标源目录 `{}` 里没有任何 SVG\n  修复：确认 `{ICON_SOURCE_DIR}` 已提交且不是空目录",
            source_dir.display()
        ));
    }
    sources
}

/// 把单色 Phosphor SVG 改写成白色字形（只改根元素的 `fill`，路径本身不动）。
///
/// 源文件里出现别的填色 / 描边时直接失败：那意味着图标不再「单色可染色」，而运行期调制
/// （`texel * modulation`）会把多色图标压成一个颜色 —— 语义变化必须在构建期暴露，不能带进界面。
fn white_glyph_svg(stem: &str, svg: &str) -> String {
    if !svg.contains(SOURCE_FILL) {
        fail(&format!(
            "图标 `{stem}` 的 SVG 里没有 `{SOURCE_FILL}`\n  修复：Phosphor 单色字形的根元素必须声明 \
             `fill=\"#000000\"`；若是多色/描边图标，需要先决定它在主题里的染色语义"
        ));
    }
    for forbidden in ["stroke=", "fill=\"url(", "style=", "<image"] {
        if svg.contains(forbidden) {
            fail(&format!(
                "图标 `{stem}` 的 SVG 含不支持的写法 `{forbidden}`\n  修复：构建期只支持纯路径单色填充 \
                 （描边 / 渐变 / 内联样式 / 内嵌图片都会让运行期调制失真）"
            ));
        }
    }
    svg.replace(SOURCE_FILL, WHITE_FILL)
}

/// 把 SVG 渲染成 `side × side` 的白色字形 PNG 字节。
fn rasterize(stem: &str, tree: &usvg::Tree, side: u32) -> Vec<u8> {
    let source = tree.size();
    if source.width() <= 0.0
        || source.height() <= 0.0
        || (source.width() - source.height()).abs() > f32::EPSILON
    {
        fail(&format!(
            "图标 `{stem}` 的 SVG 画布不是正方形（{}×{}）\n  修复：Phosphor 图标是正方形画布；\
             非正方形图标需要先补「等比 fit」策略再进生成器",
            source.width(),
            source.height()
        ));
    }
    // 固定缩放：边长 / 画布边长。resvg 的解析 AA 与 tiny-skia 的扫描线算法是确定性的。
    let scale = side as f32 / source.width();
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(side, side).unwrap_or_else(|| {
        fail(&format!(
            "无法分配 {side}×{side} 的图标像素缓冲（图标 `{stem}`）"
        ))
    });
    resvg::render(tree, transform, &mut pixmap.as_mut());

    // tiny-skia 的像素缓冲是**预乘** RGBA；PNG 要求**直通** alpha。字形是纯白，所以
    // 「预乘分量 = 覆盖 alpha」，反预乘后可见像素的 RGB 恒为 255。**alpha == 0 的像素同样写
    // 白色**（white matte）：引擎在 straight-alpha 空间先缩放、后预乘，透明像素的 RGB 会被
    // 插值进边缘像素；写 0 会让每个图标下采样后带一圈暗边（最坏 96/255，见模块文档）。
    let mut rgba = Vec::with_capacity((side * side * 4) as usize);
    for pixel in pixmap.pixels() {
        let alpha = pixel.alpha();
        let (red, green, blue) = if alpha == 0 {
            (MATTE_RGB, MATTE_RGB, MATTE_RGB)
        } else {
            // 反预乘（四舍五入），对白色字形恒等于 255；写通用式避免「假设恒定」。
            let unmultiply = |component: u8| -> u8 {
                let alpha = u32::from(alpha);
                ((u32::from(component) * 255 + alpha / 2) / alpha) as u8
            };
            (
                unmultiply(pixel.red()),
                unmultiply(pixel.green()),
                unmultiply(pixel.blue()),
            )
        };
        rgba.extend_from_slice(&[red, green, blue, alpha]);
    }

    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    encoder
        .write_image(&rgba, side, side, image::ExtendedColorType::Rgba8)
        .unwrap_or_else(|error| {
            fail(&format!(
                "图标 `{stem}`（{side}px）的 PNG 编码失败：{error}"
            ))
        });
    png
}

/// 写 `OUT_DIR/icons/built.rs`：已生成图标 stem 的清单（测试用来证明资产与枚举一一对应）。
fn write_manifest(out_dir: &Path, built: &[String]) {
    let mut manifest = String::from(
        "// 由 crates/base_ui/build.rs 生成：全部已光栅化图标的 stem（字母序）。不要手改。\npub const \
         BUILT_ICON_FILES: &[&str] = &[\n",
    );
    for stem in built {
        manifest.push_str("    \"");
        manifest.push_str(stem);
        manifest.push_str("\",\n");
    }
    manifest.push_str("];\n");
    let path = out_dir.join("built.rs");
    fs::write(&path, manifest)
        .unwrap_or_else(|error| fail(&format!("无法写入图标清单 `{}`：{error}", path.display())));
}

/// 构建失败：可读的错误（文件 / 原因 / 修复方向），退出码由 cargo 统一处理。
fn fail(message: &str) -> ! {
    panic!("base_ui/build.rs: {message}");
}

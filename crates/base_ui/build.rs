use std::{
    env, fs,
    path::{Path, PathBuf},
};

use image::ImageEncoder;

const ICON_SOURCE_DIR: &str = "assets/icons/phosphoricons";

const RASTER_TIERS: [u32; 2] = [48, 96];

const SOURCE_FILL: &str = "fill=\"#000000\"";

const WHITE_FILL: &str = "fill=\"#FFFFFF\"";

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
    let scale = side as f32 / source.width();
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(side, side).unwrap_or_else(|| {
        fail(&format!(
            "无法分配 {side}×{side} 的图标像素缓冲（图标 `{stem}`）"
        ))
    });
    resvg::render(tree, transform, &mut pixmap.as_mut());

    let mut rgba = Vec::with_capacity((side * side * 4) as usize);
    for pixel in pixmap.pixels() {
        let alpha = pixel.alpha();
        let (red, green, blue) = if alpha == 0 {
            (MATTE_RGB, MATTE_RGB, MATTE_RGB)
        } else {
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

fn fail(message: &str) -> ! {
    panic!("base_ui/build.rs: {message}");
}

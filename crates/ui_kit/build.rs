use std::{
    env, fs,
    path::{Path, PathBuf},
};

const ICON_SOURCE_DIR: &str = "assets/icons/phosphoricons";

const SOURCE_FILL: &str = "fill=\"#000000\"";

const WHITE_FILL: &str = "fill=\"#FFFFFF\"";

const FORBIDDEN: [&str; 4] = ["stroke=", "fill=\"url(", "style=", "<image"];

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
        let svg = white_template(&stem, &svg);
        let artifact = out_dir.join(format!("{stem}.svg"));
        fs::write(&artifact, svg.as_bytes()).unwrap_or_else(|error| {
            fail(&format!(
                "无法写入图标产物 `{}`（{} B）：{error}",
                artifact.display(),
                svg.len()
            ))
        });
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

fn white_template(stem: &str, svg: &str) -> String {
    if !svg.contains(SOURCE_FILL) {
        fail(&format!(
            "图标 `{stem}` 的 SVG 里没有 `{SOURCE_FILL}`\n  修复：Phosphor 单色字形的根元素必须声明 \
             `fill=\"#000000\"`；若是多色/描边图标，需要先决定它在主题里的染色语义"
        ));
    }
    for forbidden in FORBIDDEN {
        if svg.contains(forbidden) {
            fail(&format!(
                "图标 `{stem}` 的 SVG 含不支持的写法 `{forbidden}`\n  修复：只支持纯填充路径 \
                 （描边 / 渐变 / 内联样式 / 内嵌图片都会让运行期调制失真）"
            ));
        }
    }
    svg.replace(SOURCE_FILL, WHITE_FILL)
}

fn write_manifest(out_dir: &Path, built: &[String]) {
    let mut manifest = String::from(
        "// 由 crates/ui_kit/build.rs 生成：全部白色模板图标的 stem（字母序）。不要手改。\npub const \
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
    panic!("ui_kit/build.rs: {message}");
}

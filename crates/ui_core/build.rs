//! `ui_core` 的构建脚本：编译三套 shader + 构建期契约静态校验。
//!
//! 契约：`src/render/shaders/contract.md`；设计：`design/phase-07-shaders.md` §5/§6。
//!
//! - 零依赖（std only）：不引 `regex`（手写扫描器）、不引 `sha2`（Cargo 的 fingerprint 就是缓存）。
//! - 只编译本平台：Windows → `dxc`（DXIL）、macOS → `xcrun metal`（metallib）、Linux → `glslangValidator`（SPIR-V）。
//! - `ui_core_SKIP_SHADERS=1` 或 host ≠ target 时只写**占位产物**（P09 的 `include_bytes!` 仍可解析）。

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

/// 契约里的 15 个源文件（相对 `src/render/shaders`）。静态校验与 `rerun-if-changed` 的唯一清单。
const SOURCES: [&str; 15] = [
    "common.hlsl",
    "quad.hlsl",
    "geometry.hlsl",
    "common.metal",
    "quad.metal",
    "geometry.metal",
    "common.glsl",
    "common.frag.glsl",
    "quad.vert.glsl",
    "quad.frag.glsl",
    "geometry.ui.vert.glsl",
    "geometry.textured.vert.glsl",
    "geometry.ui.frag.glsl",
    "geometry.textured_image.frag.glsl",
    "geometry.textured_glyph.frag.glsl",
];

/// 每套语言里声明共享结构的文件。
const COMMON_FILES: [&str; 3] = ["common.hlsl", "common.metal", "common.glsl"];

/// `paint/instance.rs` 中的字段顺序（唯一事实源；三套 shader 必须一致）。
const GPU_FIELDS: [&str; 8] = [
    "bounds",
    "corner_radius",
    "color0",
    "color1",
    "params0",
    "params1",
    "uv",
    "clip_rect",
];
const CLIP_FIELDS: [&str; 3] = ["rect", "corner_radius", "meta"];
const FRAME_FIELDS: [&str; 2] = ["viewport", "params"];
const DRAW_FIELDS: [&str; 5] = ["basis", "translation", "color0", "clip_rect", "clip"];

/// 与 `paint/instance.rs` 的 `const NAME: f32` 逐一比对的常量（去掉 `KYTRIX_` 前缀）。
const CONSTANTS: [&str; 11] = [
    "KIND_QUAD",
    "KIND_GLYPH",
    "KIND_IMAGE",
    "KIND_SHADOW",
    "FLAG_OPAQUE",
    "TEXTURE_GLYPH",
    "TEXTURE_IMAGE",
    "GRADIENT_NONE",
    "GRADIENT_HORIZONTAL",
    "GRADIENT_VERTICAL",
    "GRADIENT_RADIAL",
];

/// 禁止出现在任何 shader 源码里的位重解释构造（contract §9）。
const FORBIDDEN: [&str; 6] = [
    "asuint(",
    "asfloat(",
    "as_type<",
    "floatBitsToInt",
    "floatBitsToUint",
    "bitcast",
];

/// `contract.md` 必须包含的锚点（防止契约文档被删改而构建仍通过）。
const CONTRACT_ANCHORS: [&str; 6] = [
    "TriangleStrip",
    "cull = NONE",
    "ONE_MINUS_SRC_ALPHA",
    "_UNORM_SRGB",
    "aa_padding",
    "size_of == 128",
];

/// D3D12 的 7 个 entry：(源文件, profile, 入口名, 产物名)。
const D3D12_ENTRIES: [(&str, &str, &str, &str); 7] = [
    ("quad.hlsl", "vs_6_2", "vs_main", "quad.vs.dxil"),
    ("quad.hlsl", "ps_6_2", "ps_main", "quad.ps.dxil"),
    ("geometry.hlsl", "vs_6_2", "vs_ui", "geometry.vs_ui.dxil"),
    (
        "geometry.hlsl",
        "vs_6_2",
        "vs_textured",
        "geometry.vs_textured.dxil",
    ),
    ("geometry.hlsl", "ps_6_2", "ps_ui", "geometry.ps_ui.dxil"),
    (
        "geometry.hlsl",
        "ps_6_2",
        "ps_textured_image",
        "geometry.ps_textured_image.dxil",
    ),
    (
        "geometry.hlsl",
        "ps_6_2",
        "ps_textured_glyph",
        "geometry.ps_textured_glyph.dxil",
    ),
];

/// Vulkan 的 7 个 stage 文件 → SPIR-V 产物（入口恒为 `main`）。
const VULKAN_ENTRIES: [(&str, &str); 7] = [
    ("quad.vert.glsl", "quad.vs.spv"),
    ("quad.frag.glsl", "quad.ps.spv"),
    ("geometry.ui.vert.glsl", "geometry.vs_ui.spv"),
    ("geometry.textured.vert.glsl", "geometry.vs_textured.spv"),
    ("geometry.ui.frag.glsl", "geometry.ps_ui.spv"),
    (
        "geometry.textured_image.frag.glsl",
        "geometry.ps_textured_image.spv",
    ),
    (
        "geometry.textured_glyph.frag.glsl",
        "geometry.ps_textured_glyph.spv",
    ),
];

/// Metal：每个文件一个 metallib（内含该族的全部入口）。
const METAL_LIBS: [(&str, &str); 2] = [
    ("quad.metal", "quad.metallib"),
    ("geometry.metal", "geometry.metallib"),
];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let shader_dir = manifest.join("src/render/shaders");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("shaders");

    emit_rerun(&manifest, &shader_dir);
    validate(&manifest, &shader_dir);
    build(&shader_dir, &out_dir);
}

// ---------------------------------------------------------------------------
// 构建触发
// ---------------------------------------------------------------------------

fn emit_rerun(manifest: &Path, shader_dir: &Path) {
    for source in SOURCES.iter().chain(["contract.md"].iter()) {
        println!(
            "cargo:rerun-if-changed={}",
            shader_dir.join(source).display()
        );
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("src/paint/instance.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("src/paint/scene.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("build.rs").display()
    );
    println!("cargo:rerun-if-env-changed=ui_core_SKIP_SHADERS");
    println!("cargo:rerun-if-env-changed=KYTRIX_DXC");
}

// ---------------------------------------------------------------------------
// 静态校验（永远执行，不受 SKIP 影响）
// ---------------------------------------------------------------------------

fn validate(manifest: &Path, shader_dir: &Path) {
    let instance_path = manifest.join("src/paint/instance.rs");
    let instance_src = strip_comments(&read(&instance_path));

    // 1) Rust 侧参照：字段顺序 + 类型。
    let rust_fields = rust_struct(&instance_src, "GpuPrimitive", "src/paint/instance.rs");
    let rust_names: Vec<&str> = rust_fields.iter().map(|(_, name)| name.as_str()).collect();
    if rust_names != GPU_FIELDS {
        fail(
            "src/paint/instance.rs",
            "GpuPrimitive",
            &format!("{GPU_FIELDS:?}"),
            &format!("{rust_names:?}"),
        );
    }
    if let Some((ty, _)) = rust_fields.iter().find(|(ty, _)| ty != "[f32; 4]") {
        fail("src/paint/instance.rs", "GpuPrimitive", "[f32; 4]", ty);
    }

    // 2) 三套 common.* 的结构体字段顺序。
    let mut shader_sources = Vec::new();
    for file in COMMON_FILES {
        let src = strip_comments(&read(&shader_dir.join(file)));
        check_struct(file, &src, "GpuPrimitive", &GPU_FIELDS, &["float4", "vec4"]);
        check_struct(
            file,
            &src,
            "ClipEntry",
            &CLIP_FIELDS,
            &["float4", "vec4", "uint4", "uvec4"],
        );
        check_struct(
            file,
            &src,
            "FrameUniform",
            &FRAME_FIELDS,
            &["float4", "vec4"],
        );
        check_struct(
            file,
            &src,
            "GeometryDrawUniform",
            &DRAW_FIELDS,
            &["float4", "vec4", "uint4", "uvec4"],
        );
        shader_sources.push((file, defines(&src)));
    }

    // 3) 常量：三套源码一致 + 与 Rust 一致。
    let instance_consts = rust_consts(&instance_src);
    for name in CONSTANTS {
        let key = format!("KYTRIX_{name}");
        let expected = const_value(&instance_consts, name, "src/paint/instance.rs");
        for (file, values) in &shader_sources {
            let actual = const_value(values, &key, file);
            if actual != expected {
                fail(
                    file,
                    &key,
                    &format!("{expected}（Rust {name}）"),
                    &actual.to_string(),
                );
            }
        }
    }

    // 4) MAX_CLIP_DEPTH：三套源码 == 8 == Rust `paint/scene.rs`。
    let scene_src = strip_comments(&read(&manifest.join("src/paint/scene.rs")));
    let scene_consts = rust_consts(&scene_src);
    let rust_depth = const_value(&scene_consts, "MAX_CLIP_DEPTH", "src/paint/scene.rs");
    for (file, values) in &shader_sources {
        let depth = const_value(values, "KYTRIX_MAX_CLIP_DEPTH", file);
        if depth != rust_depth {
            fail(
                file,
                "KYTRIX_MAX_CLIP_DEPTH",
                &format!("{rust_depth}（Rust paint/scene.rs）"),
                &depth.to_string(),
            );
        }
    }

    // 5) 禁用 token（位重解释）。
    for file in SOURCES {
        let src = read(&shader_dir.join(file));
        for token in FORBIDDEN {
            if src.contains(token) {
                fail(
                    file,
                    "禁用 token",
                    "不得出现（禁止位重解释，contract §9）",
                    token,
                );
            }
        }
    }

    // 6) contract.md 锚点。
    let contract = read(&shader_dir.join("contract.md"));
    for anchor in CONTRACT_ANCHORS {
        if !contract.contains(anchor) {
            fail("contract.md", "契约锚点", anchor, "缺失");
        }
    }
}

fn check_struct(file: &str, src: &str, name: &str, expected: &[&str], allowed_types: &[&str]) {
    let fields = c_struct(src, name, file);
    let names: Vec<&str> = fields.iter().map(|(_, field)| field.as_str()).collect();
    if names != expected {
        fail(file, name, &format!("{expected:?}"), &format!("{names:?}"));
    }
    if let Some((ty, _)) = fields
        .iter()
        .find(|(ty, _)| !allowed_types.contains(&ty.as_str()))
    {
        fail(file, name, &format!("{allowed_types:?}"), ty);
    }
}

// ---------------------------------------------------------------------------
// 编译
// ---------------------------------------------------------------------------

fn build(shader_dir: &Path, out_dir: &Path) {
    fs::create_dir_all(out_dir)
        .unwrap_or_else(|err| panic!("无法创建 {}：{err}", out_dir.display()));

    let target = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if skip_requested() || target != env::consts::OS {
        placeholders(out_dir, &target);
        return;
    }

    match target.as_str() {
        "windows" => build_windows(shader_dir, out_dir),
        "macos" => build_macos(shader_dir, out_dir),
        "linux" => build_linux(shader_dir, out_dir),
        other => {
            println!("cargo:warning=ui_core: 未知目标平台 {other}，shader 产物为占位符");
            placeholders(out_dir, other);
        }
    }
}

/// `ui_core_SKIP_SHADERS` 只认 `1` / `true`（大小写不敏感）；其它值视为未设置，并打印说明。
fn skip_requested() -> bool {
    match env::var("ui_core_SKIP_SHADERS") {
        Ok(value) if value == "1" || value.eq_ignore_ascii_case("true") => true,
        Ok(value) => {
            println!(
                "cargo:warning=ui_core: ui_core_SKIP_SHADERS={value} 无法识别（只认 1/true），按未设置处理"
            );
            false
        }
        Err(_) => false,
    }
}

fn build_windows(shader_dir: &Path, out_dir: &Path) {
    let dxc = find_dxc().unwrap_or_else(|| {
        panic!(
            "ui_core: 找不到 dxc.exe。\n  \
             已尝试：%WindowsSdkDir%\\bin\\10.0.*\\<arch>\\dxc.exe、\
             %ProgramFiles(x86)%\\Windows Kits\\10\\bin\\10.0.*\\<arch>\\dxc.exe、PATH\n  \
             安装 Windows SDK 10.0.26100+，或设置 KYTRIX_DXC=<dxc.exe 的绝对路径>；\n  \
             仅为跨 target 类型检查时可设 ui_core_SKIP_SHADERS=1（产物是占位符，不可运行）。"
        )
    });

    for (source, profile, entry, artifact) in D3D12_ENTRIES {
        let output = out_dir.join(artifact);
        let status = Command::new(&dxc)
            .args([
                "-T", profile, "-E", entry, "-HV", "2018", "-WX", "-nologo", "-Fo",
            ])
            .arg(&output)
            .arg(shader_dir.join(source))
            .output();
        report(status, &format!("dxc {entry} ({source})"));
        check_magic(&output, b"DXBC");
    }
}

fn build_macos(shader_dir: &Path, out_dir: &Path) {
    require_tool(
        "xcrun",
        "安装 Xcode Command Line Tools（`xcode-select --install`）",
    );

    for (source, artifact) in METAL_LIBS {
        let air = out_dir.join(format!("{source}.air"));
        let output = out_dir.join(artifact);
        let status = Command::new("xcrun")
            .args(["-sdk", "macosx", "metal", "-c"])
            .arg(shader_dir.join(source))
            .arg("-o")
            .arg(&air)
            .output();
        report(status, &format!("metal -c {source}"));
        let status = Command::new("xcrun")
            .args(["-sdk", "macosx", "metallib"])
            .arg(&air)
            .arg("-o")
            .arg(&output)
            .output();
        report(status, &format!("metallib {source}"));
        let _ = fs::remove_file(&air);
        check_non_empty(&output);
    }
}

fn build_linux(shader_dir: &Path, out_dir: &Path) {
    require_tool(
        "glslangValidator",
        "安装 glslang（Debian/Ubuntu: `apt install glslang-tools`）",
    );

    for (source, artifact) in VULKAN_ENTRIES {
        let output = out_dir.join(artifact);
        // glslang 的 `-I` **必须**紧贴目录（`-I<dir>`）；写成两个参数会直接报
        // `-I<dir> include path must immediately follow option (no spaces)` 并退出 1。
        let status = Command::new("glslangValidator")
            .args(["-V", "--target-env", "vulkan1.2"])
            .arg(format!("-I{}", shader_dir.display()))
            .arg("-o")
            .arg(&output)
            .arg(shader_dir.join(source))
            .output();
        report(status, &format!("glslangValidator {source}"));
        check_magic(&output, &[0x03, 0x02, 0x23, 0x07]);
    }
}

fn placeholders(out_dir: &Path, target: &str) {
    println!(
        "cargo:warning=ui_core: 跳过 shader 编译（ui_core_SKIP_SHADERS 或 host≠target），产物为占位符"
    );
    let names: Vec<&str> = match target {
        "windows" => D3D12_ENTRIES.iter().map(|entry| entry.3).collect(),
        "linux" => VULKAN_ENTRIES.iter().map(|entry| entry.1).collect(),
        "macos" => METAL_LIBS.iter().map(|entry| entry.1).collect(),
        _ => Vec::new(),
    };
    for name in names {
        fs::write(out_dir.join(name), b"KYTRIX-SHADER-SKIPPED\n")
            .unwrap_or_else(|err| panic!("无法写入占位产物：{err}"));
    }
}

fn find_dxc() -> Option<PathBuf> {
    if let Some(raw) = env::var_os("KYTRIX_DXC") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
        panic!("ui_core: KYTRIX_DXC 指向的文件不存在：{}", path.display());
    }

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let mut roots = Vec::new();
    if let Some(dir) = env::var_os("WindowsSdkDir") {
        roots.push(PathBuf::from(dir));
    }
    if let Some(dir) = env::var_os("ProgramFiles(x86)") {
        roots.push(PathBuf::from(dir).join("Windows Kits").join("10"));
    }
    if let Some(dir) = env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(dir).join("Windows Kits").join("10"));
    }
    for root in roots {
        if let Some(found) = newest_dxc(&root, arch) {
            return Some(found);
        }
    }

    let on_path = PathBuf::from("dxc.exe");
    if Command::new(&on_path).arg("--version").output().is_ok() {
        return Some(on_path);
    }
    None
}

/// 在 `<root>/bin/<version>/<arch>/dxc.exe` 中取版本号最大的一个。
fn newest_dxc(root: &Path, arch: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root.join("bin")).ok()?;
    let mut versions: Vec<(Vec<u32>, PathBuf)> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?.to_owned();
            let key: Vec<u32> = name
                .split('.')
                .map(|part| part.parse().ok())
                .collect::<Option<Vec<u32>>>()?;
            Some((key, path))
        })
        .collect();
    versions.sort_by(|left, right| right.0.cmp(&left.0));
    versions
        .into_iter()
        .map(|(_, dir)| dir.join(arch).join("dxc.exe"))
        .find(|candidate| candidate.is_file())
}

fn require_tool(tool: &str, hint: &str) {
    if Command::new(tool).arg("--version").output().is_err() {
        panic!("ui_core: 找不到 {tool}。{hint}；或设 ui_core_SKIP_SHADERS=1 只做类型检查。");
    }
}

fn report(output: std::io::Result<std::process::Output>, what: &str) {
    let output = output.unwrap_or_else(|err| panic!("ui_core: 无法启动编译器（{what}）：{err}"));
    if !output.status.success() {
        panic!(
            "ui_core: 着色器编译失败（{what}）\n  status: {}\n  stdout:\n{}\n  stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn check_magic(path: &Path, magic: &[u8; 4]) {
    let bytes =
        fs::read(path).unwrap_or_else(|err| panic!("无法读取产物 {}：{err}", path.display()));
    if bytes.len() < magic.len() || bytes[..4] != *magic {
        panic!(
            "ui_core: 产物 magic 不符：{}（期望 {:02X?}）",
            path.display(),
            magic
        );
    }
}

fn check_non_empty(path: &Path) {
    let bytes =
        fs::read(path).unwrap_or_else(|err| panic!("无法读取产物 {}：{err}", path.display()));
    if bytes.len() < 16 {
        panic!("ui_core: 产物为空或过小：{}", path.display());
    }
}

// ---------------------------------------------------------------------------
// 文本解析（手写扫描器；无 regex）
// ---------------------------------------------------------------------------

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("无法读取 {}：{err}", path.display()))
}

/// 剥离 `//` 行注释（用空格替换、保留换行）。必须先做：`float4 bounds; // 112` 的分号会把注释粘到下一字段。
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        match line.find("//") {
            Some(index) => {
                out.push_str(&line[..index]);
                out.extend(
                    line[index..]
                        .chars()
                        .map(|ch| if ch == '\n' { '\n' } else { ' ' }),
                );
            }
            None => out.push_str(line),
        }
    }
    out
}

fn struct_body<'a>(src: &'a str, name: &str, file: &str) -> &'a str {
    let needle = format!("struct {name}");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| fail(file, name, "存在该结构体声明", "找不到"));
    let open = start
        + src[start..]
            .find('{')
            .unwrap_or_else(|| fail(file, name, "结构体带 `{`", "找不到"));
    let mut depth = 0i32;
    for (offset, ch) in src[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    fail(file, name, "结构体闭合", "未闭合")
}

/// C 系（HLSL / MSL / GLSL）：按 `;` 切分，每段「类型 字段名」。
fn c_struct(src: &str, name: &str, file: &str) -> Vec<(String, String)> {
    struct_body(src, name, file)
        .split(';')
        .map(str::trim)
        .filter(|decl| !decl.is_empty())
        .map(|decl| {
            let mut parts = decl.split_whitespace();
            let ty = parts
                .next()
                .unwrap_or_else(|| fail(file, name, "类型", "空声明"))
                .to_owned();
            let field = parts
                .next()
                .unwrap_or_else(|| fail(file, name, "字段名", decl))
                .to_owned();
            if parts.next().is_some() {
                fail(file, name, "两个 token 的字段声明", decl);
            }
            (ty, field)
        })
        .collect()
}

/// Rust：按 `,` 切分，剥 `pub `，按 `:` 切（`[f32; 4]` 里有分号，绝不能按 `;` 切）。
fn rust_struct(src: &str, name: &str, file: &str) -> Vec<(String, String)> {
    struct_body(src, name, file)
        .split(',')
        .map(str::trim)
        .filter(|decl| !decl.is_empty())
        .map(|decl| {
            let decl = decl.strip_prefix("pub ").unwrap_or(decl);
            let (field, ty) = decl
                .split_once(':')
                .unwrap_or_else(|| fail(file, name, "`字段: 类型`", decl));
            (ty.trim().to_owned(), field.trim().to_owned())
        })
        .collect()
}

fn defines(src: &str) -> Vec<(String, f32)> {
    src.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("#define ")?;
            let mut parts = rest.split_whitespace();
            let name = parts.next()?.to_owned();
            let value = number(parts.next()?)?;
            Some((name, value))
        })
        .collect()
}

fn rust_consts(src: &str) -> Vec<(String, f32)> {
    src.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line
                .strip_prefix("pub(crate) const ")
                .or_else(|| line.strip_prefix("const "))?;
            let (name, tail) = rest.split_once(':')?;
            let value = tail.split('=').nth(1)?.trim().trim_end_matches(';').trim();
            Some((name.trim().to_owned(), number(value)?))
        })
        .collect()
}

fn number(token: &str) -> Option<f32> {
    token
        .trim_end_matches(['u', 'U', 'f', 'F'])
        .parse::<f32>()
        .ok()
}

fn const_value(values: &[(String, f32)], name: &str, file: &str) -> f32 {
    values
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| *value)
        .unwrap_or_else(|| fail(file, name, "存在该常量", "找不到"))
}

fn fail(file: &str, item: &str, expected: &str, actual: &str) -> ! {
    panic!(
        "\nui_core: 着色器契约漂移\n  文件:   {file}\n  对象:   {item}\n  期望:   {expected}\n  实际:   {actual}\n  修复:   同时改 Rust 与三套源码，并跑像素快照回归（src/render/shaders/contract.md）\n"
    )
}

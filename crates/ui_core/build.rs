use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

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

const COMMON_FILES: [&str; 3] = ["common.hlsl", "common.metal", "common.glsl"];

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

const CONSTANTS: [&str; 13] = [
    "KIND_QUAD",
    "KIND_GLYPH",
    "KIND_IMAGE",
    "KIND_SHADOW",
    "GRADIENT_NONE",
    "GRADIENT_HORIZONTAL",
    "GRADIENT_VERTICAL",
    "GRADIENT_RADIAL",
    "CLIP_KIND_RECT",
    "CLIP_KIND_ROUNDED",
    "MAX_CLIP_ITEMS",
    "SHADOW_SIGMA_SCALE",
    "SHADOW_SIGMA_MIN",
];

const SHADER_ONLY_CONSTANTS: [&str; 2] = ["KYTRIX_CLIP_VERTEX_SLACK", "KYTRIX_AA_INSIDE"];

const FORBIDDEN: [&str; 6] = [
    "asuint(",
    "asfloat(",
    "as_type<",
    "floatBitsToInt",
    "floatBitsToUint",
    "bitcast",
];

const CONTRACT_ANCHORS: [&str; 7] = [
    "TriangleStrip",
    "cull = NONE",
    "ONE_MINUS_SRC_ALPHA",
    "_UNORM_SRGB",
    "aa_padding",
    "size_of == 128",
    "instance_base",
];

const INSTANCE_BASE_ANCHORS: [(&str, &str); 3] = [
    ("quad.hlsl", "instances[instance_id + base]"),
    ("quad.vert.glsl", "instances[instance]"),
    ("quad.metal", "instances[instance]"),
];

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

const METAL_LIBS: [(&str, &str); 2] = [
    ("quad.metal", "quad.metallib"),
    ("geometry.metal", "geometry.metallib"),
];

const MANIFEST_RC: &str = "src/platform/windows/ui_core_manifest.rc";
const MANIFEST_XML: &str = "src/platform/windows/ui_core.manifest.xml";

const MANIFEST_XML_ANCHORS: [&str; 2] = [">true/pm</dpiAware>", ">PerMonitorV2</dpiAwareness>"];

const MANIFEST_RC_ANCHOR: &str = "1 RT_MANIFEST";

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let shader_dir = manifest.join("src/render/shaders");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("shaders");

    emit_rerun(&manifest, &shader_dir);
    validate(&manifest, &shader_dir);
    embed_manifest(&manifest);
    build(&shader_dir, &out_dir);
}

fn emit_rerun(manifest: &Path, shader_dir: &Path) {
    for source in SOURCES.iter().chain(["contract.md"].iter()) {
        println!(
            "cargo:rerun-if-changed={}",
            shader_dir.join(source).display()
        );
    }
    let mut watched: Vec<&str> = vec!["build.rs", "src/paint/instance.rs", "src/paint/scene.rs"];
    watched.extend(RENDER_STATE_ANCHORS.iter().map(|(file, _, _)| *file));
    watched.extend(RENDER_STATE_FORBIDDEN.iter().map(|(file, _, _)| *file));
    watched.sort_unstable();
    watched.dedup();
    for source in watched {
        println!("cargo:rerun-if-changed={}", manifest.join(source).display());
    }
    println!("cargo:rerun-if-env-changed=ui_core_SKIP_SHADERS");
    println!("cargo:rerun-if-env-changed=KYTRIX_DXC");
}

fn embed_manifest(project: &Path) {
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let rc_file = project.join(MANIFEST_RC);
    let xml_file = project.join(MANIFEST_XML);
    println!("cargo:rerun-if-changed={}", rc_file.display());
    println!("cargo:rerun-if-changed={}", xml_file.display());

    let xml = read(&xml_file);
    for anchor in MANIFEST_XML_ANCHORS {
        if !xml.contains(anchor) {
            panic!(
                "\nui_core: 进程清单漂移（R07-1）\n  文件:   {MANIFEST_XML}\n  缺失:   {anchor}\n  \
                 修复:   DPI awareness 必须是 PerMonitorV2，且老/新 Windows 两条声明都要有；\
                 否则窗口跨屏时收不到每屏 DPI\n"
            );
        }
    }
    if !read(&rc_file).contains(MANIFEST_RC_ANCHOR) {
        panic!(
            "\nui_core: 进程清单漂移（R07-1）\n  文件:   {MANIFEST_RC}\n  缺失:   {MANIFEST_RC_ANCHOR}\n  \
             修复:   进程清单必须写成 `1 RT_MANIFEST`（ID ≠ 1 的资源不会被系统当作进程清单）\n"
        );
    }

    if env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() != "msvc" {
        panic!(
            "ui_core: R07-1 的进程清单只支持 MSVC 目标（需要 rc.exe）。\n  \
             当前 CARGO_CFG_TARGET_ENV 不是 msvc；请改用 *-pc-windows-msvc 目标。"
        );
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let artifact = out_dir.join("ui_core_manifest.lib");
    let rc_tool = find_rc().unwrap_or_else(|| {
        panic!(
            "ui_core: 找不到 rc.exe（把进程清单编进 exe，R07-1）。\n  \
             已尝试：%WindowsSdkDir%\\bin\\10.0.*\\<arch>\\rc.exe、\
             %ProgramFiles(x86)%\\Windows Kits\\10\\bin\\10.0.*\\<arch>\\rc.exe、PATH\n  \
             安装 Windows SDK 10.0.26100+，或设置 KYTRIX_RC=<rc.exe 的绝对路径>。"
        )
    });
    let output = Command::new(&rc_tool)
        .args(["/nologo", "/fo"])
        .arg(&artifact)
        .arg("/I")
        .arg(project.join("src/platform/windows"))
        .arg(&rc_file)
        .current_dir(project)
        .output();
    report(output, "rc.exe（进程清单）");

    let bytes = fs::read(&artifact)
        .unwrap_or_else(|err| panic!("无法读取进程清单产物 {}：{err}", artifact.display()));
    if !bytes.starts_with(b"\0\0\0\0\x20\0\0\0") {
        panic!(
            "ui_core: rc.exe 没有产出 .res 资源（magic 不符）：{}",
            artifact.display()
        );
    }

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=dylib=ui_core_manifest");
}

fn validate(manifest: &Path, shader_dir: &Path) {
    let instance_path = manifest.join("src/paint/instance.rs");
    let instance_src = strip_comments(&read(&instance_path));

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
        let items = const_value(values, "KYTRIX_MAX_CLIP_ITEMS", file);
        if items != depth + 1.0 {
            fail(
                file,
                "KYTRIX_MAX_CLIP_ITEMS",
                &format!("{}（= KYTRIX_MAX_CLIP_DEPTH + 1）", depth + 1.0),
                &items.to_string(),
            );
        }
    }

    for name in SHADER_ONLY_CONSTANTS {
        let mut reference: Option<(&str, f32)> = None;
        for (file, values) in &shader_sources {
            let value = const_value(values, name, file);
            match reference {
                None => reference = Some((file, value)),
                Some((expected_file, expected)) if value != expected => fail(
                    file,
                    name,
                    &format!("{expected}（同 {expected_file}）"),
                    &value.to_string(),
                ),
                Some(_) => {}
            }
        }
    }

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

    let contract = read(&shader_dir.join("contract.md"));
    for anchor in CONTRACT_ANCHORS {
        if !contract.contains(anchor) {
            fail("contract.md", "契约锚点", anchor, "缺失");
        }
    }

    for (file, anchor) in INSTANCE_BASE_ANCHORS {
        let src = read(&shader_dir.join(file));
        if !src.contains(anchor) {
            fail(
                file,
                "实例基址",
                &format!("包含 `{anchor}`（经 draw.clip.y 加偏移）"),
                "找不到（回退到硬件 baseInstance 会三平台分叉）",
            );
        }
        if !src.contains("draw.clip.y") && !src.contains("root.draw.clip.y") {
            fail(
                file,
                "实例基址",
                "读取 `draw.clip.y` 作为 instance_base",
                "找不到",
            );
        }
    }

    validate_math_equivalence(shader_dir);

    validate_render_state(manifest);
}

fn validate_math_equivalence(shader_dir: &Path) {
    let load = |files: &[&str]| {
        files
            .iter()
            .map(|file| ((*file).to_owned(), read(&shader_dir.join(file))))
            .collect::<Vec<_>>()
    };
    let hlsl = ShaderSource::new(
        Language::Hlsl,
        load(&["common.hlsl", "geometry.hlsl", "quad.hlsl"]),
    );
    let metal = ShaderSource::new(
        Language::Metal,
        load(&["common.metal", "geometry.metal", "quad.metal"]),
    );
    let glsl = ShaderSource::new(
        Language::Glsl,
        load(&[
            "common.glsl",
            "common.frag.glsl",
            "geometry.ui.vert.glsl",
            "geometry.ui.frag.glsl",
            "geometry.textured.vert.glsl",
            "geometry.textured_image.frag.glsl",
            "geometry.textured_glyph.frag.glsl",
            "quad.vert.glsl",
            "quad.frag.glsl",
        ]),
    );
    if let Err(mismatch) = compare_math_functions(&hlsl, &[&metal, &glsl]) {
        panic!(
            "\nui_core: 三套 shader 的数学函数体不一致（R06-5）{mismatch}\n  \
             修复: 三份源码必须逐语句等价（只允许语言拼写差异：类型名 / lerp↔mix / root. 前缀）\n  \
             契约: src/render/shaders/contract.md §4\n"
        );
    }
}

const RENDER_STATE_ANCHORS: &[(&str, &str, &str)] = &[
    (
        "src/render/windows/pipeline.rs",
        "blend enable",
        "BlendEnable: true.into()",
    ),
    (
        "src/render/windows/pipeline.rs",
        "blend src",
        "SrcBlend: D3D12_BLEND_ONE",
    ),
    (
        "src/render/windows/pipeline.rs",
        "blend dst",
        "DestBlend: D3D12_BLEND_INV_SRC_ALPHA",
    ),
    (
        "src/render/windows/pipeline.rs",
        "blend op",
        "BlendOp: D3D12_BLEND_OP_ADD",
    ),
    (
        "src/render/windows/pipeline.rs",
        "alpha src",
        "SrcBlendAlpha: D3D12_BLEND_ONE",
    ),
    (
        "src/render/windows/pipeline.rs",
        "alpha dst",
        "DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA",
    ),
    (
        "src/render/windows/pipeline.rs",
        "alpha op",
        "BlendOpAlpha: D3D12_BLEND_OP_ADD",
    ),
    (
        "src/render/windows/pipeline.rs",
        "颜色写掩码",
        "RenderTargetWriteMask: 0x0F",
    ),
    (
        "src/render/windows/pipeline.rs",
        "cull",
        "CullMode: D3D12_CULL_MODE_NONE",
    ),
    (
        "src/render/windows/pipeline.rs",
        "fill",
        "FillMode: D3D12_FILL_MODE_SOLID",
    ),
    (
        "src/render/windows/pipeline.rs",
        "quad 拓扑",
        "D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP",
    ),
    (
        "src/render/windows/pipeline.rs",
        "RT 格式（sRGB）",
        "rtv_formats[0] = DXGI_FORMAT_B8G8R8A8_UNORM_SRGB",
    ),
    (
        "src/render/windows/pipeline.rs",
        "MSAA",
        "SampleDesc: DXGI_SAMPLE_DESC {",
    ),
    (
        "src/render/windows/pipeline.rs",
        "geometry Ui 覆盖率属性",
        "DXGI_FORMAT_R32_FLOAT, 0, 8",
    ),
    (
        "src/render/windows/swapchain.rs",
        "呈现面字节格式",
        "Format: DXGI_FORMAT_B8G8R8A8_UNORM",
    ),
    (
        "src/render/windows/swapchain.rs",
        "呈现色彩空间（R06-10）",
        "SetColorSpace1",
    ),
    (
        "src/render/macos/pipeline.rs",
        "blend enable",
        "attachment.setBlendingEnabled(true)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "blend src",
        "setSourceRGBBlendFactor(MTLBlendFactor::One)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "blend dst",
        "setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "blend op",
        "setRgbBlendOperation(MTLBlendOperation::Add)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "alpha src",
        "setSourceAlphaBlendFactor(MTLBlendFactor::One)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "alpha dst",
        "setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "alpha op",
        "setAlphaBlendOperation(MTLBlendOperation::Add)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "颜色写掩码",
        "setWriteMask(MTLColorWriteMask::All)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "geometry Ui 顶点布局（12 B）",
        "Some(12)",
    ),
    (
        "src/render/macos/swapchain.rs",
        "呈现面格式（sRGB）",
        "MTLPixelFormat::BGRA8Unorm_sRGB",
    ),
    (
        "src/render/macos/swapchain.rs",
        "呈现色彩空间（R06-10）",
        "kCGColorSpaceSRGB",
    ),
    (
        "src/render/linux/pipeline.rs",
        "blend enable",
        ".blend_enable(true)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "blend src",
        ".src_color_blend_factor(vk::BlendFactor::ONE)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "blend dst",
        ".dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "blend op",
        ".color_blend_op(vk::BlendOp::ADD)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "alpha src",
        ".src_alpha_blend_factor(vk::BlendFactor::ONE)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "alpha dst",
        ".dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "alpha op",
        ".alpha_blend_op(vk::BlendOp::ADD)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "颜色写掩码",
        ".color_write_mask(vk::ColorComponentFlags::RGBA)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "cull",
        ".cull_mode(vk::CullModeFlags::NONE)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "fill",
        ".polygon_mode(vk::PolygonMode::FILL)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "quad 拓扑",
        ".topology(vk::PrimitiveTopology::TRIANGLE_STRIP)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "RT 格式（sRGB）",
        "vk::Format::B8G8R8A8_SRGB",
    ),
    (
        "src/render/linux/pipeline.rs",
        "MSAA",
        ".samples(vk::SampleCountFlags::TYPE_1)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "geometry Ui 覆盖率属性",
        "12 => vec![position, coverage]",
    ),
    (
        "src/render/linux/swapchain.rs",
        "呈现色彩空间（R06-10）",
        "SRGB_NONLINEAR",
    ),
    (
        "src/render/colorspace.rs",
        "唯一呈现色彩空间声明",
        "PRESENTED_COLOR_SPACE: PresentedColorSpace = PresentedColorSpace::SrgbRec709",
    ),
];

const RENDER_STATE_FORBIDDEN: &[(&str, &str, &str)] = &[
    (
        "src/render/macos/pipeline.rs",
        "cull（Metal 默认 none，不得显式设置）",
        "setCullMode",
    ),
    (
        "src/render/macos/pipeline.rs",
        "fill 模式（Metal 默认 fill）",
        "setTriangleFillMode",
    ),
    (
        "src/render/windows/pipeline.rs",
        "cull 回退",
        "D3D12_CULL_MODE_BACK",
    ),
    (
        "src/render/linux/pipeline.rs",
        "cull 回退",
        "vk::CullModeFlags::BACK",
    ),
];

fn validate_render_state(manifest: &Path) {
    for (file, what, expected) in RENDER_STATE_ANCHORS {
        let contents = read(&manifest.join(file));
        if let Err(detail) = check_anchors(file, &contents, &[(what, expected)]) {
            panic!(
                "\nui_core: 渲染状态漂移（R06-5）\n  {detail}\n  \
                 修复: 三端渲染状态必须语义一致（contract.md §7）；改一处必须三端同步\n"
            );
        }
    }
    for (file, what, token) in RENDER_STATE_FORBIDDEN {
        let contents = read(&manifest.join(file));
        if let Err(detail) = check_forbidden(file, &contents, &[(what, token)]) {
            panic!(
                "\nui_core: 渲染状态漂移（R06-5）\n  {detail}\n  \
                 修复: 三端渲染状态必须语义一致（contract.md §7）\n"
            );
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

    find_windows_sdk_tool("dxc.exe").or_else(|| {
        let on_path = PathBuf::from("dxc.exe");
        Command::new(&on_path)
            .arg("--version")
            .output()
            .is_ok()
            .then_some(on_path)
    })
}

fn find_rc() -> Option<PathBuf> {
    if let Some(raw) = env::var_os("KYTRIX_RC") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
        panic!("ui_core: KYTRIX_RC 指向的文件不存在：{}", path.display());
    }

    find_windows_sdk_tool("rc.exe").or_else(|| {
        let on_path = PathBuf::from("rc.exe");
        Command::new(&on_path)
            .arg("/?")
            .output()
            .is_ok()
            .then_some(on_path)
    })
}

fn find_windows_sdk_tool(tool: &str) -> Option<PathBuf> {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "x86"
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
    roots
        .into_iter()
        .find_map(|root| newest_sdk_tool(&root, arch, tool))
}

fn newest_sdk_tool(root: &Path, arch: &str, tool: &str) -> Option<PathBuf> {
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
        .map(|(_, dir)| dir.join(arch).join(tool))
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

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("无法读取 {}：{err}", path.display()))
}

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

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/render/shader_contract.rs"
));
